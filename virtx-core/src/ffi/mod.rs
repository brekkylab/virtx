//! [`Client`](crate::client::Client) behind a C ABI, as `include/virtx.h` declares it.
//!
//! What every type behind the ABI shares: the runtime it runs on and C holds it by, the call and its waker, and
//! the strings across the ABI. Each type's own functions are next to it, as the client's are
//! in `client`.
//!
//! Opening a client blocks, returns [`VIRTX_OK`] or [`VIRTX_ERROR`], and writes the client or
//! the error's message. A request returns a [`VirtxCall`] at once and runs on the client's own
//! runtime; C drives it as a Rust future is driven: [`virtx_call_poll`] answers
//! [`VIRTX_PENDING`] and keeps the [`VirtxWaker`] it was given, which the call wakes once it
//! has an answer, and the next poll takes it. So the caller's event loop, whatever it is, never
//! blocks on one: only a waker's `wake` runs on another thread. [`virtx_call_wait`] blocks
//! instead, for a caller without a loop. A call that sends progress, as `image.build` does,
//! queues each and wakes the waker for it: [`virtx_call_progress`] takes them, until none is
//! left, before the next poll.
//!
//! A client is shared by reference count: `virtx_client_retain` for each holder, and
//! `virtx_client_release` by each, the last of which ends the session.

mod async_call;
mod runtime;
mod text;

use std::ffi::c_int;

pub use async_call::*;
pub(crate) use runtime::{Runtime, release, retain, share};
pub use text::virtx_string_free;
pub(crate) use text::{json, text};

/// Succeeded: the output, as JSON, is written.
pub const VIRTX_OK: c_int = 0;

/// Failed: the error's message is written.
pub const VIRTX_ERROR: c_int = -1;

/// Not answered yet: the waker given is kept, and woken once it is.
pub const VIRTX_PENDING: c_int = 1;
