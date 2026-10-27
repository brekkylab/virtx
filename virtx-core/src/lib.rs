//! A virtx client behind a C ABI.
//!
//! The client is an opaque handle: C holds only the pointer `virtx_client_spawn` or
//! `virtx_client_connect` writes, shared by `virtx_client_retain` and `virtx_client_release`. What
//! follows is the same over every transport, so only those two name one. `include/virtx.h`
//! declares these functions, and changes with them.
//!
//! The types every request takes and answers are always built; the wire's client end is the
//! `client` feature, its server end the `server` feature, and the C ABI over a client the `ffi`
//! feature, on by default with `client`. A crate that reaches the library through the C ABI
//! takes the types alone, with `default-features = false`; a server takes `server`.

#[cfg(feature = "ffi")]
mod client;
#[cfg(feature = "ffi")]
mod ffi;
mod image;
mod method;
mod sandbox;
mod snapshot;
#[cfg(any(feature = "client", feature = "server"))]
mod transport;
mod volume;

pub use image::*;
pub use method::*;
pub use sandbox::*;
pub use snapshot::*;
#[cfg(any(feature = "client", feature = "server"))]
pub use transport::*;
pub use volume::*;
