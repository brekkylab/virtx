//! A request in flight: a `VirtxFuture` as a Rust [`Future`], a `VirtxStream` as a [`Stream`],
//! each polled with the task's own waker.

use std::{
    ffi::{c_int, c_void},
    future::Future,
    marker::PhantomData,
    pin::Pin,
    slice,
    task::{Context, Poll, Waker},
};

use futures_core::Stream;
use serde::de::DeserializeOwned;
use virtx_core::Event;

use crate::{
    Error,
    library::{
        Api, Opaque, VIRTX_END, VIRTX_ERROR, VIRTX_OK, VIRTX_PENDING, VirtxBytes, VirtxWaker,
    },
};

/// A request that answers once with a `T`. It runs from when it is made, whether awaited or
/// not; dropping it cancels it.
#[must_use = "a call dropped is cancelled"]
pub struct Call<T> {
    api: &'static Api,
    future: *mut Opaque,
    output: PhantomData<fn() -> T>,
}

// The library's futures may be polled from any thread, one at a time, which `&mut self` ensures.
unsafe impl<T> Send for Call<T> {}
unsafe impl<T> Sync for Call<T> {}

impl<T> Unpin for Call<T> {}

impl<T: DeserializeOwned> Call<T> {
    pub(crate) fn new(api: &'static Api, future: *mut Opaque) -> Call<T> {
        Call {
            api,
            future,
            output: PhantomData,
        }
    }

    /// Block this thread until the call answers, for a caller without an async runtime.
    pub fn wait(self) -> Result<T, Error> {
        let mut out = VirtxBytes::empty();
        let status = unsafe { (self.api.virtx_future_wait)(self.future, &mut out) };
        unsafe { take(self.api, status, out) }
    }
}

impl<T: DeserializeOwned> Future for Call<T> {
    type Output = Result<T, Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut out = VirtxBytes::empty();
        let status = unsafe { (self.api.virtx_future_poll)(self.future, waker(cx), &mut out) };
        match status {
            VIRTX_PENDING => Poll::Pending,
            status => Poll::Ready(unsafe { take(self.api, status, out) }),
        }
    }
}

impl<T> Drop for Call<T> {
    fn drop(&mut self) {
        unsafe { (self.api.virtx_future_free)(self.future) };
    }
}

/// A request that answers item by item: its progress, each with a `C` as its content, then
/// its output, a `T`, or the error it failed with. It runs from when it is made; dropping it
/// cancels it.
#[must_use = "a call dropped is cancelled"]
pub struct Events<T, C> {
    api: &'static Api,
    stream: *mut Opaque,
    output: PhantomData<fn() -> (T, C)>,
}

// As `Call`.
unsafe impl<T, C> Send for Events<T, C> {}
unsafe impl<T, C> Sync for Events<T, C> {}

impl<T, C> Unpin for Events<T, C> {}

impl<T: DeserializeOwned, C: DeserializeOwned> Events<T, C> {
    pub(crate) fn new(api: &'static Api, stream: *mut Opaque) -> Events<T, C> {
        Events {
            api,
            stream,
            output: PhantomData,
        }
    }

    /// Block this thread until the next item, or `None` once there is none, for a caller
    /// without an async runtime.
    pub fn wait_next(&mut self) -> Option<Result<Event<T, C>, Error>> {
        let mut out = VirtxBytes::empty();
        let status = unsafe { (self.api.virtx_stream_wait_next)(self.stream, &mut out) };
        match status {
            VIRTX_END => None,
            status => Some(unsafe { take(self.api, status, out) }),
        }
    }
}

impl<T: DeserializeOwned, C: DeserializeOwned> Stream for Events<T, C> {
    type Item = Result<Event<T, C>, Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut out = VirtxBytes::empty();
        let status = unsafe { (self.api.virtx_stream_poll_next)(self.stream, waker(cx), &mut out) };
        match status {
            VIRTX_PENDING => Poll::Pending,
            VIRTX_END => Poll::Ready(None),
            status => Poll::Ready(Some(unsafe { take(self.api, status, out) })),
        }
    }
}

impl<T, C> Drop for Events<T, C> {
    fn drop(&mut self) {
        unsafe { (self.api.virtx_stream_free)(self.stream) };
    }
}

/// The task's waker, as the library takes one: woken from the library's threads, released once.
fn waker(cx: &Context<'_>) -> VirtxWaker {
    unsafe extern "C" fn wake(data: *mut c_void) {
        unsafe { &*data.cast::<Waker>() }.wake_by_ref();
    }
    unsafe extern "C" fn release(data: *mut c_void) {
        drop(unsafe { Box::from_raw(data.cast::<Waker>()) });
    }
    VirtxWaker {
        data: Box::into_raw(Box::new(cx.waker().clone())).cast(),
        wake,
        release: Some(release),
    }
}

/// What the library wrote for `status`, read as a `T` or as the error's message, and freed.
///
/// # Safety
///
/// `out` is as `api` wrote it with `status`, not yet freed.
pub(crate) unsafe fn take<T: DeserializeOwned>(
    api: &Api,
    status: c_int,
    out: VirtxBytes,
) -> Result<T, Error> {
    let read = if out.data.is_null() {
        Err(Error::Json("the library wrote nothing".into()))
    } else {
        let bytes = unsafe { slice::from_raw_parts(out.data, out.len) };
        match status {
            VIRTX_OK => serde_json::from_slice(bytes).map_err(|e| Error::Json(e.to_string())),
            VIRTX_ERROR => Err(match serde_json::from_slice::<String>(bytes) {
                Ok(message) => Error::Call(message),
                Err(e) => Error::Json(e.to_string()),
            }),
            other => Err(Error::Library(format!("unknown status {other}"))),
        }
    };
    unsafe { (api.virtx_bytes_free)(out) };
    read
}
