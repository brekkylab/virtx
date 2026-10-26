//! A request that answers item by item, polled by C as a Rust stream is.

use std::{
    collections::VecDeque,
    ffi::c_int,
    mem,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
};

use futures_util::{Stream, StreamExt};
use serde::Serialize;
use tokio::{
    runtime::Handle,
    task::{AbortHandle, JoinSet},
};

use super::{
    VIRTX_END, VIRTX_ERROR, VIRTX_PENDING,
    bytes::{VirtxBytes, encode, failure, write},
    waker::{VirtxWaker, Waker, block_on},
};

/// A request that answers item by item: polled for each until it ends, then freed.
pub struct VirtxStream {
    shared: Arc<Mutex<Shared>>,
    task: AbortHandle,
}

#[derive(Default)]
struct Shared {
    /// Items not yet taken, oldest first: a status, and the item or the error.
    items: VecDeque<(c_int, Vec<u8>)>,
    /// No item comes after those in `items`.
    ended: bool,
    /// The waker of the last poll that found nothing to take.
    waker: Option<Waker>,
}

fn lock(shared: &Mutex<Shared>) -> MutexGuard<'_, Shared> {
    shared.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Where a stream's task puts its items. Dropped before it ends the stream (its task cancelled
/// by the client's release, or panicked), it ends it with an error.
struct Items(Arc<Mutex<Shared>>);

impl Items {
    /// Queue an item, or end the stream after it if `last`, and wake the waker kept.
    fn push(&self, item: Option<(c_int, Vec<u8>)>, last: bool) {
        let waker = {
            let mut shared = lock(&self.0);
            if shared.ended {
                return;
            }
            shared.items.extend(item);
            shared.ended = last;
            shared.waker.take()
        };
        // Outside the lock, so a `wake` that polls at once finds the item.
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

impl Drop for Items {
    fn drop(&mut self) {
        self.push(Some(failure("the call ended before it finished")), true);
    }
}

/// Run `stream` on `runtime`, tracked in `tasks`, and hand C the stream. Its first error is
/// its last item.
pub(super) fn start<T: Serialize>(
    runtime: &Handle,
    tasks: &Mutex<JoinSet<()>>,
    stream: impl Stream<Item = Result<T, String>> + Send + 'static,
) -> *mut VirtxStream {
    let shared = Arc::new(Mutex::new(Shared::default()));
    let items = Items(Arc::clone(&shared));
    let mut tasks = tasks.lock().unwrap_or_else(PoisonError::into_inner);
    // Finished tasks stay in the set until reaped.
    while tasks.try_join_next().is_some() {}
    let task = tasks.spawn_on(
        async move {
            let mut stream = std::pin::pin!(stream);
            while let Some(item) = stream.next().await {
                let (status, item) = encode(item);
                let failed = status == VIRTX_ERROR;
                items.push(Some((status, item)), failed);
                if failed {
                    return;
                }
            }
            items.push(None, true);
        },
        runtime,
    );
    Box::into_raw(Box::new(VirtxStream { shared, task }))
}

/// Take `stream`'s next item: `VIRTX_OK`, with it written to `out` as JSON; `VIRTX_ERROR`,
/// with the error written, after which the stream ends; `VIRTX_END` once it has; or
/// `VIRTX_PENDING`, keeping `waker` (and releasing the one kept before) to wake once there is
/// an item or the end.
///
/// # Safety
///
/// `stream` is one a request returned, not yet freed, and polled by one thread at a time.
/// `waker` is as `VirtxWaker` says. `out` is null or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn virtx_stream_poll_next(
    stream: *mut VirtxStream,
    waker: VirtxWaker,
    out: *mut VirtxBytes,
) -> c_int {
    let waker = Waker::new(waker);
    let mut shared = lock(&unsafe { &*stream }.shared);
    let Some((status, item)) = shared.items.pop_front() else {
        if shared.ended {
            drop(shared);
            drop(waker);
            return VIRTX_END;
        }
        let kept = shared.waker.replace(waker);
        drop(shared);
        drop(kept);
        return VIRTX_PENDING;
    };
    drop(shared);
    drop(waker);
    unsafe { write(out, item) };
    status
}

/// Block until `stream` has an item or has ended, and take it as `virtx_stream_poll_next`
/// does.
///
/// # Safety
///
/// As `virtx_stream_poll_next`. Not called from a `wake`, which would block the runtime it
/// runs on.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn virtx_stream_wait_next(
    stream: *mut VirtxStream,
    out: *mut VirtxBytes,
) -> c_int {
    block_on(|waker| unsafe { virtx_stream_poll_next(stream, waker, out) })
}

/// Free `stream`, cancelling it if it has not ended, and release its waker without waking it.
/// A `wake` already under way may still finish after this returns. Null does nothing.
///
/// # Safety
///
/// `stream` is null, or one a request returned, not yet freed; it is not used again.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn virtx_stream_free(stream: *mut VirtxStream) {
    if stream.is_null() {
        return;
    }
    let stream = unsafe { Box::from_raw(stream) };
    let kept = {
        let mut shared = lock(&stream.shared);
        shared.ended = true;
        mem::take(&mut shared.waker)
    };
    drop(kept);
    stream.task.abort();
}

#[cfg(test)]
mod tests {
    use std::{thread, time::Duration};

    use serde_json::Value;
    use tokio::{runtime::Runtime, sync::mpsc};

    use super::{
        super::{
            VIRTX_OK,
            waker::testing::{Counts, empty, taken, waker},
        },
        *,
    };

    fn poll(stream: *mut VirtxStream, counts: &Arc<Counts>) -> (c_int, Option<Value>) {
        let mut out = empty();
        let status = unsafe { virtx_stream_poll_next(stream, waker(counts), &mut out) };
        (status, unsafe { taken(out) })
    }

    fn wait(stream: *mut VirtxStream) -> (c_int, Option<Value>) {
        let mut out = empty();
        let status = unsafe { virtx_stream_wait_next(stream, &mut out) };
        (status, unsafe { taken(out) })
    }

    /// A stream of what `send` is given, until it is dropped.
    fn pending(
        runtime: &Runtime,
        tasks: &Mutex<JoinSet<()>>,
    ) -> (*mut VirtxStream, mpsc::UnboundedSender<Result<u32, String>>) {
        let (send, mut receive) = mpsc::unbounded_channel();
        let items = async_stream::stream! {
            while let Some(item) = receive.recv().await {
                yield item;
            }
        };
        (start(runtime.handle(), tasks, items), send)
    }

    #[test]
    fn a_stream_wakes_for_each_item_and_ends() {
        let runtime = Runtime::new().unwrap();
        let tasks = Mutex::new(JoinSet::new());
        let (stream, send) = pending(&runtime, &tasks);
        let counts: Arc<Counts> = Arc::default();

        assert_eq!(poll(stream, &counts), (VIRTX_PENDING, None));
        send.send(Ok(1)).unwrap();
        counts.await_wakes(1);
        assert_eq!(poll(stream, &counts), (VIRTX_OK, Some(Value::from(1))));

        assert_eq!(poll(stream, &counts), (VIRTX_PENDING, None));
        send.send(Ok(2)).unwrap();
        drop(send);
        counts.await_wakes(2);
        assert_eq!(wait(stream), (VIRTX_OK, Some(Value::from(2))));
        assert_eq!(wait(stream), (VIRTX_END, None));
        assert_eq!(poll(stream, &counts), (VIRTX_END, None));
        assert_eq!(counts.get(), (2, 4), "every waker released once");

        unsafe { virtx_stream_free(stream) };
    }

    #[test]
    fn an_error_is_a_stream_s_last_item() {
        let runtime = Runtime::new().unwrap();
        let tasks = Mutex::new(JoinSet::new());
        let (stream, send) = pending(&runtime, &tasks);

        send.send(Err("no".to_string())).unwrap();
        assert_eq!(wait(stream), (VIRTX_ERROR, Some(Value::from("no"))));
        let _ = send.send(Ok(1));
        assert_eq!(wait(stream), (VIRTX_END, None));

        unsafe { virtx_stream_free(stream) };
    }

    #[test]
    fn a_stream_cancelled_with_its_client_ends_with_an_error() {
        let runtime = Runtime::new().unwrap();
        let tasks = Mutex::new(JoinSet::new());
        let (stream, _send) = pending(&runtime, &tasks);
        let counts = Arc::default();

        assert_eq!(poll(stream, &counts), (VIRTX_PENDING, None));
        runtime.block_on(tasks.lock().unwrap().shutdown());
        assert_eq!(counts.get(), (1, 1));
        assert_eq!(poll(stream, &counts).0, VIRTX_ERROR);
        assert_eq!(poll(stream, &counts), (VIRTX_END, None));

        unsafe { virtx_stream_free(stream) };
    }

    #[test]
    fn a_freed_stream_releases_its_waker_unwoken() {
        let runtime = Runtime::new().unwrap();
        let tasks = Mutex::new(JoinSet::new());
        let (stream, send) = pending(&runtime, &tasks);
        let counts = Arc::default();

        assert_eq!(poll(stream, &counts), (VIRTX_PENDING, None));
        unsafe { virtx_stream_free(stream) };
        let _ = send.send(Ok(1));
        runtime.block_on(tasks.into_inner().unwrap().shutdown());
        assert_eq!(counts.get(), (0, 1));
    }

    #[test]
    fn waiting_blocks_until_an_item_comes() {
        let runtime = Runtime::new().unwrap();
        let tasks = Mutex::new(JoinSet::new());
        let (stream, send) = pending(&runtime, &tasks);

        let sender = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            send.send(Ok(7)).unwrap();
        });
        assert_eq!(wait(stream), (VIRTX_OK, Some(Value::from(7))));

        sender.join().unwrap();
        unsafe { virtx_stream_free(stream) };
    }
}
