//! A virtx client behind a C ABI.
//!
//! The client is an opaque handle: C holds only the pointer `virtx_client_spawn` or
//! `virtx_client_connect` writes, shared by `virtx_client_retain` and `virtx_client_release`. What
//! follows is the same over every transport, so only those two name one. `include/virtx.h`
//! declares these functions, and changes with them.

mod client;
mod ffi;
mod image;
mod method;
mod transport;
mod volume;

pub use image::*;
pub use volume::*;
