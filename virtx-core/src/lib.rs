//! An MCP client behind a C ABI.
//!
//! The client is an opaque handle: C holds only the pointer a `virtx_client_init_*`
//! returns, and hands it back to `virtx_client_free` exactly once. What follows the
//! handshake is the same over every transport, so only `init` names one. `include/virtx.h`
//! declares these functions, and changes with them.

mod client;
pub mod image;
pub mod method;
pub mod transport;

pub use client::Client;
