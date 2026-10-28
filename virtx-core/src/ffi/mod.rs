//! [`Client`](crate::client::Client) behind a C ABI, as `include/virtx.h` declares it.
//!
//! What every type behind the ABI shares: the runtime it runs on and C holds it by, the futures
//! and streams its requests return and the waker they wake, and the JSON across the ABI. Each
//! type's own functions are next to it, as the client's are in `client`.
//!
//! Opening a client blocks, returns [`VIRTX_OK`] or [`VIRTX_ERROR`], and writes the client or
//! the error's message. A request takes JSON and returns at once, running on the client's own
//! runtime: a [`VirtxFuture`] if it answers once, a [`VirtxStream`] if it answers item by item.
//! What the library writes is JSON in a [`VirtxBytes`]: an output, or an error's message as a
//! string. C drives futures and streams as Rust does: [`virtx_future_poll`] and
//! [`virtx_stream_poll_next`] answer [`VIRTX_PENDING`] and keep the [`VirtxWaker`](waker::VirtxWaker) they were
//! given, which is woken once there is something to take, and the next poll takes it. So the
//! caller's event loop, whatever it is, never blocks on one: only a waker's `wake` runs on
//! another thread. [`virtx_future_wait`] and [`virtx_stream_wait_next`] block instead, for a
//! caller without a loop.
//!
//! A client is shared by reference count: `virtx_client_retain` for each holder, and
//! `virtx_client_release` by each, the last of which ends the session.

mod bytes;
mod future;
mod runtime;
mod stream;
mod waker;

use std::ffi::c_int;

pub use bytes::VirtxBytes;
#[cfg(test)]
pub(crate) use bytes::virtx_bytes_free;
pub(crate) use bytes::{encode, json, write};
pub use future::*;
pub(crate) use runtime::{Runtime, release, retain, share};
pub use stream::*;

/// Succeeded: the output, as JSON, is written.
pub const VIRTX_OK: c_int = 0;

/// Failed: the error's message is written, as a JSON string.
pub const VIRTX_ERROR: c_int = -1;

/// Not answered yet: the waker given is kept, and woken once it is.
pub const VIRTX_PENDING: c_int = 1;

/// A stream has ended: it has no item left.
pub const VIRTX_END: c_int = 2;
