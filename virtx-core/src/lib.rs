//! An MCP client behind a C ABI.
//!
//! The client is an opaque handle: C holds only the pointer `virtx_client_spawn` or
//! `virtx_client_connect` writes, and hands it back to `virtx_client_free` exactly once. What
//! follows is the same over every transport, so only those two name one. `include/virtx.h`
//! declares these functions, and changes with them.

mod client;
pub mod ffi;
pub mod image;
pub mod method;
pub mod transport;

pub use client::Client;
