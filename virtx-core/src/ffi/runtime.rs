//! A Rust value behind the C ABI, on a tokio runtime of its own: C calls in without one, so
//! the value brings the runtime its calls run on, and cancels them when it ends. Shared by
//! reference count, since more than one holder (two native modules, say) may hold it.
//!
//! A type C holds wraps a [`Runtime`] in a struct of its own, so the header names it and keeps
//! it opaque, and passes that struct to [`share`], [`retain`] and [`release`].

use std::{
    ffi::{c_char, c_int},
    future::Future,
    sync::{Arc, Mutex, PoisonError},
};

use serde::Serialize;
use tokio::task::JoinSet;

use super::{
    VIRTX_ERROR, VIRTX_OK,
    async_call::{VirtxCall, start},
    text::write,
};

/// `T`, the runtime its calls run on (C calls in without one), and those calls, which
/// [`Runtime::close`] cancels.
pub(crate) struct Runtime<T> {
    runtime: tokio::runtime::Runtime,
    value: Arc<T>,
    calls: Mutex<JoinSet<()>>,
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
            calls: Mutex::new(JoinSet::new()),
        })
    }

    /// Start what `call` makes of the value, and hand C the call.
    pub(crate) fn call<R: Serialize, F>(&self, call: impl FnOnce(Arc<T>) -> F) -> *mut VirtxCall
    where
        F: Future<Output = Result<R, String>> + Send + 'static,
    {
        start(
            self.runtime.handle(),
            &self.calls,
            call(Arc::clone(&self.value)),
        )
    }

    /// Answer every call not yet answered with `VIRTX_ERROR`, then end the value with `close`.
    ///
    /// Not called on the runtime's own threads, which this stops.
    pub(crate) fn close<F: Future<Output = ()>>(self, close: impl FnOnce(T) -> F) {
        let Runtime {
            runtime,
            value,
            calls,
        } = self;
        let mut calls = calls.into_inner().unwrap_or_else(PoisonError::into_inner);
        runtime.block_on(async move {
            calls.shutdown().await;
            // Every call's clone went with its task.
            if let Ok(value) = Arc::try_unwrap(value) {
                close(value).await;
            }
        });
    }
}

/// Write `opened`, held once, to `out`, or its error to `error`.
///
/// # Safety
///
/// `out` is writable. `error` is null or writable.
pub(crate) unsafe fn share<H>(
    opened: Result<H, String>,
    out: *mut *mut H,
    error: *mut *mut c_char,
) -> c_int {
    match opened {
        Ok(opened) => {
            unsafe { *out = Arc::into_raw(Arc::new(opened)).cast_mut() };
            VIRTX_OK
        }
        Err(message) => {
            unsafe { write(error, message) };
            VIRTX_ERROR
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
        super::{async_call::virtx_call_free, virtx_call_wait, virtx_string_free},
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
    fn closing_answers_the_calls_in_flight() {
        let closes = Arc::default();
        let held = shared(&closes);
        let call = unsafe { &(*held).0 }.call(|_| std::future::pending::<Result<(), String>>());

        assert!(close(held));
        assert_eq!(closes.load(SeqCst), 1, "the call's clone went with it");
        let mut out = ptr::null_mut();
        assert_eq!(unsafe { virtx_call_wait(call, &mut out) }, VIRTX_ERROR);

        unsafe { virtx_string_free(out) };
        unsafe { virtx_call_free(call) };
    }
}
