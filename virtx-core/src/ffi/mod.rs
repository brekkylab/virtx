//! [`Client`](crate::client::Client) behind a C ABI, as `include/virtx.h` declares it.
//!
//! What every type behind the ABI shares: the runtime it runs on and C holds it by, the futures
//! and streams its requests return and the waker they wake, and the strings across the ABI.
//! Each type's own functions are next to it, as the client's are in `client`.
//!
//! Opening a client blocks, returns [`VIRTX_OK`] or [`VIRTX_ERROR`], and writes the client or
//! the error's message. A request returns at once and runs on the client's own runtime: a
//! [`VirtxFuture`] if it answers once, a [`VirtxStream`] if it answers item by item. C drives
//! them as Rust drives a future and a stream: [`virtx_future_poll`] and
//! [`virtx_stream_poll_next`] answer [`VIRTX_PENDING`] and keep the [`VirtxWaker`] they were
//! given, which is woken once there is something to take, and the next poll takes it. So the
//! caller's event loop, whatever it is, never blocks on one: only a waker's `wake` runs on
//! another thread. [`virtx_future_wait`] and [`virtx_stream_wait_next`] block instead, for a
//! caller without a loop.
//!
//! A client is shared by reference count: `virtx_client_retain` for each holder, and
//! `virtx_client_release` by each, the last of which ends the session.

mod future;
mod runtime;
mod stream;
mod text;
mod waker;

use std::ffi::c_int;

pub use future::*;
pub(crate) use runtime::{Runtime, release, retain, share};
pub use stream::*;
pub use text::virtx_string_free;
pub(crate) use text::{json, text};
pub use waker::VirtxWaker;

/// Succeeded: the output, as JSON, is written.
pub const VIRTX_OK: c_int = 0;

/// Failed: the error's message is written.
pub const VIRTX_ERROR: c_int = -1;

/// Not answered yet: the waker given is kept, and woken once it is.
pub const VIRTX_PENDING: c_int = 1;

/// A stream has ended: it has no item left.
pub const VIRTX_END: c_int = 2;
