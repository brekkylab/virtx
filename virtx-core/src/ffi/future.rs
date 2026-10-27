//! A request that answers once, polled by C as a Rust future is.

use std::{
    ffi::c_int,
    future::Future,
    mem,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
};

use serde::Serialize;
use tokio::{
    runtime::Handle,
    task::{AbortHandle, JoinSet},
};

use super::{
    VIRTX_PENDING,
    bytes::{VirtxBytes, encode, failure, write},
    waker::{VirtxWaker, Waker, block_on},
};

/// A request that answers once: polled until it answers, then freed.
pub struct VirtxFuture {
    state: Arc<Mutex<State>>,
    task: AbortHandle,
}

enum State {
    /// Running, with the waker of the last poll.
    Pending(Option<Waker>),
    /// Answered, not yet polled.
    Ready(c_int, Vec<u8>),
    /// Answered and polled, or freed.
    Taken,
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Answers a future: by `finish`, or with an error when dropped first (its task cancelled by
/// the client's release, or panicked).
struct Completion(Arc<Mutex<State>>);

impl Completion {
    fn finish(&self, (status, output): (c_int, Vec<u8>)) {
        let waker = {
            let state = &mut *lock(&self.0);
            match mem::replace(state, State::Taken) {
                State::Pending(waker) => {
                    *state = State::Ready(status, output);
                    waker
                }
                done => {
                    *state = done;
                    None
                }
            }
        };
        // Outside the lock, so a `wake` that polls at once finds the answer.
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

impl Drop for Completion {
    fn drop(&mut self) {
        self.finish(failure("the call ended before it answered"));
    }
}

/// Run `future` on `runtime`, tracked in `tasks`, and hand C the future.
pub(super) fn start<T: Serialize>(
    runtime: &Handle,
    tasks: &Mutex<JoinSet<()>>,
    future: impl Future<Output = Result<T, String>> + Send + 'static,
) -> *mut VirtxFuture {
    let state = Arc::new(Mutex::new(State::Pending(None)));
    let completion = Completion(Arc::clone(&state));
    let mut tasks = tasks.lock().unwrap_or_else(PoisonError::into_inner);
    // Finished tasks stay in the set until reaped.
    while tasks.try_join_next().is_some() {}
    let task = tasks.spawn_on(
        async move {
            completion.finish(encode(future.await));
        },
        runtime,
    );
    Box::into_raw(Box::new(VirtxFuture { state, task }))
}

/// Take `future`'s answer: `VIRTX_OK` or `VIRTX_ERROR`, with the output or the error written
/// to `out`, or `VIRTX_PENDING`, keeping `waker` (and releasing the one kept before) to wake
/// once there is one. A future answers once; polled again, it fails.
///
/// # Safety
///
/// `future` is one a request returned, not yet freed, and polled by one thread at a time.
/// `waker` is as `VirtxWaker` says. `out` is null or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn virtx_future_poll(
    future: *mut VirtxFuture,
    waker: VirtxWaker,
    out: *mut VirtxBytes,
) -> c_int {
    let waker = Waker::new(waker);
    let mut state = lock(&unsafe { &*future }.state);
    let (status, output) = match mem::replace(&mut *state, State::Taken) {
        State::Pending(kept) => {
            *state = State::Pending(Some(waker));
            drop(state);
            drop(kept);
            return VIRTX_PENDING;
        }
        State::Ready(status, output) => (status, output),
        State::Taken => failure("the call already answered"),
    };
    drop(state);
    drop(waker);
    unsafe { write(out, output) };
    status
}

/// Block until `future` answers, and take its answer as `virtx_future_poll` does.
///
/// # Safety
///
/// As `virtx_future_poll`. Not called from a `wake`, which would block the runtime it runs on.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn virtx_future_wait(
    future: *mut VirtxFuture,
    out: *mut VirtxBytes,
) -> c_int {
    block_on(|waker| unsafe { virtx_future_poll(future, waker, out) })
}

/// Free `future`, cancelling it if it has not answered, and release its waker without waking
/// it. A `wake` already under way may still finish after this returns. Null does nothing.
///
/// # Safety
///
/// `future` is null, or one a request returned, not yet freed; it is not used again.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn virtx_future_free(future: *mut VirtxFuture) {
    if future.is_null() {
        return;
    }
    let future = unsafe { Box::from_raw(future) };
    let kept = mem::replace(&mut *lock(&future.state), State::Taken);
    drop(kept);
    future.task.abort();
}

#[cfg(test)]
mod tests {
    use std::{thread, time::Duration};

    use serde_json::Value;
    use tokio::{runtime::Runtime, sync::oneshot};

    use super::{
        super::{
            VIRTX_ERROR,
            waker::testing::{Counts, empty, taken, waker},
        },
        *,
    };

    fn poll(future: *mut VirtxFuture, counts: &Arc<Counts>) -> (c_int, Option<Value>) {
        let mut out = empty();
        let status = unsafe { virtx_future_poll(future, waker(counts), &mut out) };
        (status, unsafe { taken(out) })
    }

    /// A future that answers what `send` is given.
    fn pending(
        runtime: &Runtime,
        tasks: &Mutex<JoinSet<()>>,
    ) -> (*mut VirtxFuture, oneshot::Sender<u32>) {
        let (send, receive) = oneshot::channel();
        let future = start(runtime.handle(), tasks, async move {
            receive.await.map_err(|e| e.to_string())
        });
        (future, send)
    }

    #[test]
    fn a_future_wakes_its_last_waker_once_and_answers_once() {
        let runtime = Runtime::new().unwrap();
        let tasks = Mutex::new(JoinSet::new());
        let (future, send) = pending(&runtime, &tasks);
        let (first, last, after): (Arc<Counts>, Arc<Counts>, Arc<Counts>) = Default::default();

        assert_eq!(poll(future, &first), (VIRTX_PENDING, None));
        assert_eq!(poll(future, &last), (VIRTX_PENDING, None));
        assert_eq!(first.get(), (0, 1), "replaced, so released unwoken");

        send.send(7).unwrap();
        last.await_wakes(1);
        assert_eq!(last.get(), (1, 1));

        assert_eq!(poll(future, &after), (0, Some(Value::from(7))));
        assert_eq!(poll(future, &after).0, VIRTX_ERROR);
        assert_eq!(after.get(), (0, 2), "a ready poll keeps no waker");

        unsafe { virtx_future_free(future) };
    }

    #[test]
    fn a_freed_future_releases_its_waker_unwoken() {
        let runtime = Runtime::new().unwrap();
        let tasks = Mutex::new(JoinSet::new());
        let (future, send) = pending(&runtime, &tasks);
        let counts = Arc::default();

        assert_eq!(poll(future, &counts), (VIRTX_PENDING, None));
        unsafe { virtx_future_free(future) };
        let _ = send.send(7);
        runtime.block_on(tasks.into_inner().unwrap().shutdown());
        assert_eq!(counts.get(), (0, 1));
    }

    #[test]
    fn a_future_cancelled_with_its_client_answers_with_an_error() {
        let runtime = Runtime::new().unwrap();
        let tasks = Mutex::new(JoinSet::new());
        let (future, _send) = pending(&runtime, &tasks);
        let counts = Arc::default();

        assert_eq!(poll(future, &counts), (VIRTX_PENDING, None));
        runtime.block_on(tasks.lock().unwrap().shutdown());
        assert_eq!(counts.get(), (1, 1));
        assert_eq!(poll(future, &counts).0, VIRTX_ERROR);

        unsafe { virtx_future_free(future) };
    }

    #[test]
    fn waiting_blocks_until_the_future_answers() {
        let runtime = Runtime::new().unwrap();
        let tasks = Mutex::new(JoinSet::new());
        let (future, send) = pending(&runtime, &tasks);

        let sender = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            send.send(7).unwrap();
        });
        let mut out = empty();
        let status = unsafe { virtx_future_wait(future, &mut out) };
        assert_eq!((status, unsafe { taken(out) }), (0, Some(Value::from(7))));

        sender.join().unwrap();
        unsafe { virtx_future_free(future) };
    }
}
