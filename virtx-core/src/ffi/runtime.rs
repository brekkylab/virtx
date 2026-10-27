//! A Rust value behind the C ABI, on a tokio runtime of its own: C calls in without one, so
//! the value brings the runtime its requests run on, and cancels them when it ends. Shared by
//! reference count, since more than one holder (two native modules, say) may hold it.
//!
//! A type C holds wraps a [`Runtime`] in a struct of its own, so the header names it and keeps
//! it opaque, and passes that struct to [`share`], [`retain`] and [`release`].

use std::{
    ffi::c_int,
    future::Future,
    sync::{Arc, Mutex, PoisonError},
};

use futures_util::Stream;
use serde::Serialize;
use tokio::task::JoinSet;

use super::{
    VIRTX_OK,
    bytes::{VirtxBytes, failure, write},
    future::{self, VirtxFuture},
    stream::{self, VirtxStream},
};

/// `T`, the runtime its requests run on (C calls in without one), and the tasks running them,
/// which [`Runtime::close`] cancels.
pub(crate) struct Runtime<T> {
    runtime: tokio::runtime::Runtime,
    value: Arc<T>,
    tasks: Mutex<JoinSet<()>>,
}

impl<T: Send + Sync + 'static> Runtime<T> {
    /// What `make` makes, inside a runtime of its own.
    pub(crate) fn open(make: impl FnOnce() -> Result<T, String>) -> Result<Self, String> {
        let runtime = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
        let value = {
            let _entered = runtime.enter();
            make()?
        };
        Ok(Runtime {
            runtime,
            value: Arc::new(value),
            tasks: Mutex::new(JoinSet::new()),
        })
    }

    /// Start what `make` makes of the value, and hand C the future.
    pub(crate) fn future<R: Serialize, F>(&self, make: impl FnOnce(Arc<T>) -> F) -> *mut VirtxFuture
    where
        F: Future<Output = Result<R, String>> + Send + 'static,
    {
        future::start(
            self.runtime.handle(),
            &self.tasks,
            make(Arc::clone(&self.value)),
        )
    }

    /// Start what `make` makes of the value, and hand C the stream.
    pub(crate) fn stream<R: Serialize, S>(&self, make: impl FnOnce(Arc<T>) -> S) -> *mut VirtxStream
    where
        S: Stream<Item = Result<R, String>> + Send + 'static,
    {
        stream::start(
            self.runtime.handle(),
            &self.tasks,
            make(Arc::clone(&self.value)),
        )
    }

    /// Answer every request not yet answered with `VIRTX_ERROR`, then end the value with
    /// `close`.
    ///
    /// Not called on the runtime's own threads, which this stops.
    pub(crate) fn close<F: Future<Output = ()>>(self, close: impl FnOnce(T) -> F) {
        let Runtime {
            runtime,
            value,
            tasks,
        } = self;
        let mut tasks = tasks.into_inner().unwrap_or_else(PoisonError::into_inner);
        runtime.block_on(async move {
            tasks.shutdown().await;
            // Every request's clone went with its task.
            if let Ok(value) = Arc::try_unwrap(value) {
                close(value).await;
            }
        });
    }
}

/// Write `opened`, held once, to `out`, or its error's message, as a JSON string, to
/// `error`.
///
/// # Safety
///
/// `out` is writable. `error` is null or writable.
pub(crate) unsafe fn share<H>(
    opened: Result<H, String>,
    out: *mut *mut H,
    error: *mut VirtxBytes,
) -> c_int {
    match opened {
        Ok(opened) => {
            unsafe { *out = Arc::into_raw(Arc::new(opened)).cast_mut() };
            VIRTX_OK
        }
        Err(message) => {
            let (status, message) = failure(message);
            unsafe { write(error, message) };
            status
        }
    }
}

/// Hold `held` once more.
///
/// # Safety
///
/// `held` is one `share` wrote, still held.
pub(crate) unsafe fn retain<H>(held: *mut H) {
    unsafe { Arc::increment_strong_count(held) };
}

/// Let go of `held`: its value if this was the last hold, for the caller to close. Null is
/// not held.
///
/// # Safety
///
/// `held` is null, or one `share` wrote, held by the caller, who does not use it again.
pub(crate) unsafe fn release<H>(held: *mut H) -> Option<H> {
    if held.is_null() {
        return None;
    }
    Arc::into_inner(unsafe { Arc::from_raw(held) })
}

#[cfg(test)]
mod tests {
    use std::{
        ptr,
        sync::atomic::{AtomicUsize, Ordering::SeqCst},
    };

    use super::{
        super::{
            VIRTX_END, VIRTX_ERROR, virtx_bytes_free, virtx_future_free, virtx_future_wait,
            virtx_stream_free, virtx_stream_wait_next, waker::testing::empty,
        },
        *,
    };

    struct Held(Runtime<Arc<AtomicUsize>>);

    fn shared(closes: &Arc<AtomicUsize>) -> *mut Held {
        let mut held = ptr::null_mut();
        let opened = Runtime::open(|| Ok(Arc::clone(closes))).map(Held);
        assert_eq!(
            unsafe { share(opened, &mut held, ptr::null_mut()) },
            VIRTX_OK
        );
        held
    }

    fn close(held: *mut Held) -> bool {
        match unsafe { release(held) } {
            Some(Held(runtime)) => {
                runtime.close(async |closes| {
                    closes.fetch_add(1, SeqCst);
                });
                true
            }
            None => false,
        }
    }

    #[test]
    fn only_the_last_release_closes() {
        let closes = Arc::default();
        let held = shared(&closes);
        unsafe { retain(held) };

        assert!(!close(held));
        assert_eq!(closes.load(SeqCst), 0);
        assert!(close(held));
        assert_eq!(closes.load(SeqCst), 1);
        assert!(!close(ptr::null_mut()));
    }

    #[test]
    fn closing_answers_the_requests_in_flight() {
        let closes = Arc::default();
        let held = shared(&closes);
        let runtime = unsafe { &(*held).0 };
        let future = runtime.future(|value| async move {
            let _held = value;
            std::future::pending::<Result<(), String>>().await
        });
        let stream = runtime.stream(|value| {
            async_stream::stream! {
                let _held = value;
                std::future::pending::<()>().await;
                yield Ok::<(), String>(());
            }
        });

        assert!(close(held));
        assert_eq!(closes.load(SeqCst), 1, "each request's clone went with it");
        let mut out = empty();
        assert_eq!(unsafe { virtx_future_wait(future, &mut out) }, VIRTX_ERROR);
        unsafe { virtx_bytes_free(out) };
        let mut out = empty();
        assert_eq!(
            unsafe { virtx_stream_wait_next(stream, &mut out) },
            VIRTX_ERROR
        );
        unsafe { virtx_bytes_free(out) };
        let mut out = empty();
        assert_eq!(
            unsafe { virtx_stream_wait_next(stream, &mut out) },
            VIRTX_END
        );

        unsafe { virtx_future_free(future) };
        unsafe { virtx_stream_free(stream) };
    }
}
