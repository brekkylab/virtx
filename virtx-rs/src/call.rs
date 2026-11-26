//! A request in flight: a `VirtxFuture` as a Rust [`Future`], a `VirtxStream` as a [`Stream`],
//! each polled with the task's own waker.

use std::{
    ffi::{c_int, c_void},
    future::Future,
    marker::PhantomData,
    mem::ManuallyDrop,
    pin::Pin,
    ptr, slice,
    task::{Context, Poll, Waker},
};

use futures_core::Stream;
use serde::de::DeserializeOwned;

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
    /// What the library writes, read as a `T`.
    decode: Decode<T>,
    output: PhantomData<fn() -> T>,
}

type Decode<T> = Box<dyn Fn(&[u8]) -> Result<T, Error> + Send + Sync>;

// The library's futures may be polled from any thread, one at a time, which `&mut self` ensures.
unsafe impl<T> Send for Call<T> {}
unsafe impl<T> Sync for Call<T> {}

impl<T> Unpin for Call<T> {}

impl<T: DeserializeOwned + 'static> Call<T> {
    /// A call answered with JSON, read as a `T`.
    pub(crate) fn new(api: &'static Api, future: *mut Opaque) -> Call<T> {
        Call {
            api,
            future,
            decode: Box::new(json),
            output: PhantomData,
        }
    }
}

impl Call<Vec<u8>> {
    /// A call answered with bytes as they are.
    pub(crate) fn raw(api: &'static Api, future: *mut Opaque) -> Call<Vec<u8>> {
        Call {
            api,
            future,
            decode: Box::new(|bytes| Ok(bytes.to_vec())),
            output: PhantomData,
        }
    }
}

impl<T> Call<T> {
    /// Block this thread until the call answers, for a caller without an async runtime.
    pub fn wait(self) -> Result<T, Error> {
        let mut out = VirtxBytes::empty();
        let status = unsafe { (self.api.virtx_future_wait)(self.future, &mut out) };
        unsafe { take_with(self.api, status, out, &self.decode) }
    }

    /// The same call, answered with `f` of what it answers.
    pub(crate) fn map<U>(self, f: impl Fn(T) -> U + Send + Sync + 'static) -> Call<U>
    where
        T: 'static,
    {
        // Its future moves to the new call, so this one must not free it.
        let this = ManuallyDrop::new(self);
        let decode = unsafe { ptr::read(&this.decode) };
        Call {
            api: this.api,
            future: this.future,
            decode: Box::new(move |bytes| decode(bytes).map(&f)),
            output: PhantomData,
        }
    }
}

impl<T> Future for Call<T> {
    type Output = Result<T, Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut out = VirtxBytes::empty();
        let status = unsafe { (self.api.virtx_future_poll)(self.future, waker(cx), &mut out) };
        match status {
            VIRTX_PENDING => Poll::Pending,
            status => Poll::Ready(unsafe { take_with(self.api, status, out, &self.decode) }),
        }
    }
}

impl<T> Drop for Call<T> {
    fn drop(&mut self) {
        unsafe { (self.api.virtx_future_free)(self.future) };
    }
}

/// A request that answers item by item, each a `T`, or the error it failed with. It runs from
/// when it is made; dropping it cancels it.
#[must_use = "a call dropped is cancelled"]
pub struct Events<T> {
    api: &'static Api,
    stream: *mut Opaque,
    output: PhantomData<fn() -> T>,
}

// As `Call`.
unsafe impl<T> Send for Events<T> {}
unsafe impl<T> Sync for Events<T> {}

impl<T> Unpin for Events<T> {}

impl<T: DeserializeOwned> Events<T> {
    pub(crate) fn new(api: &'static Api, stream: *mut Opaque) -> Events<T> {
        Events {
            api,
            stream,
            output: PhantomData,
        }
    }

    /// Block this thread until the next item, or `None` once there is none, for a caller
    /// without an async runtime.
    pub fn wait_next(&mut self) -> Option<Result<T, Error>> {
        let mut out = VirtxBytes::empty();
        let status = unsafe { (self.api.virtx_stream_wait_next)(self.stream, &mut out) };
        match status {
            VIRTX_END => None,
            status => Some(unsafe { take(self.api, status, out) }),
        }
    }
}

impl<T: DeserializeOwned> Stream for Events<T> {
    type Item = Result<T, Error>;

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

impl<T> Drop for Events<T> {
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

/// `bytes` as JSON, read as a `T`.
fn json<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, Error> {
    serde_json::from_slice(bytes).map_err(|e| Error::Json(e.to_string()))
}

/// What the library wrote for `status`, read as a `T` from JSON or as the error's message,
/// and freed.
///
/// # Safety
///
/// `out` is as `api` wrote it with `status`, not yet freed.
pub(crate) unsafe fn take<T: DeserializeOwned>(
    api: &Api,
    status: c_int,
    out: VirtxBytes,
) -> Result<T, Error> {
    unsafe { take_with(api, status, out, &json) }
}

/// What the library wrote for `status`, read with `decode` or as the error's message, and
/// freed.
///
/// What the library writes with `VIRTX_ERROR`.
#[derive(serde::Deserialize)]
struct Failure {
    message: String,
    source: virtx_core::Source,
    status: Option<u16>,
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    written: Option<u64>,
}

/// # Safety
///
/// As `take`.
unsafe fn take_with<T>(
    api: &Api,
    status: c_int,
    out: VirtxBytes,
    decode: &dyn Fn(&[u8]) -> Result<T, Error>,
) -> Result<T, Error> {
    let read = if out.data.is_null() {
        Err(Error::Json("the library wrote nothing".into()))
    } else {
        let bytes = unsafe { slice::from_raw_parts(out.data, out.len) };
        match status {
            VIRTX_OK => decode(bytes),
            VIRTX_ERROR => Err(match serde_json::from_slice::<Failure>(bytes) {
                Ok(Failure {
                    message,
                    status: Some(status),
                    code,
                    written,
                    ..
                }) => Error::Status {
                    status,
                    code,
                    message,
                    written,
                },
                Ok(Failure {
                    message, source, ..
                }) => Error::Call(source, message),
                Err(e) => Error::Json(e.to_string()),
            }),
            other => Err(Error::Library(format!("unknown status {other}"))),
        }
    };
    unsafe { (api.virtx_bytes_free)(out) };
    read
}

/// A request answered with its stream's first item, made into a `T` that holds the stream from
/// then on: what the stream keeps going, such as a sandbox's client ports taken, goes on for as
/// long as the `T` is held. It runs from when it is made; dropping it before it answers cancels
/// it.
#[must_use = "a call dropped is cancelled"]
pub struct Holding<T> {
    events: Option<Events<serde_json::Value>>,
    make: Make<T>,
}

type Make<T> =
    Box<dyn Fn(serde_json::Value, Events<serde_json::Value>) -> Result<T, Error> + Send + Sync>;

impl<T> Unpin for Holding<T> {}

impl<T> Holding<T> {
    pub(crate) fn new(
        events: Events<serde_json::Value>,
        make: impl Fn(serde_json::Value, Events<serde_json::Value>) -> Result<T, Error>
        + Send
        + Sync
        + 'static,
    ) -> Holding<T> {
        Holding {
            events: Some(events),
            make: Box::new(make),
        }
    }

    /// Block this thread until the call answers, for a caller without an async runtime.
    pub fn wait(mut self) -> Result<T, Error> {
        let mut events = self.events.take().expect("answered once");
        let first = events.wait_next();
        self.answer(first, events)
    }

    fn answer(
        &self,
        first: Option<Result<serde_json::Value, Error>>,
        events: Events<serde_json::Value>,
    ) -> Result<T, Error> {
        match first {
            Some(Ok(first)) => (self.make)(first, events),
            Some(Err(e)) => Err(e),
            None => Err(Error::Call(
                virtx_core::Source::Transport,
                "the call ended before it answered".into(),
            )),
        }
    }
}

impl<T> Future for Holding<T> {
    type Output = Result<T, Error>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let events = self.events.as_mut().expect("polled after it answered");
        let first = match Pin::new(events).poll_next(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(first) => first,
        };
        let events = self.events.take().expect("held until it answers");
        Poll::Ready(self.answer(first, events))
    }
}
