//! A virtx client behind a C ABI.
//!
//! The client is an opaque handle: C holds only the pointer `virtx_client_spawn` or
//! `virtx_client_connect` writes, shared by `virtx_client_retain` and `virtx_client_release`. What
//! follows is the same over every transport, so only those two name one. `include/virtx.h`
//! declares these functions, and changes with them.
//!
//! The types every request takes and answers are always built; the wire is the `transport`
//! feature, and the C ABI over it the `ffi` feature, both on by default. A crate that reaches
//! the library through the C ABI takes the types alone, with `default-features = false`.

#[cfg(feature = "ffi")]
mod client;
#[cfg(feature = "ffi")]
mod ffi;
mod image;
mod method;
#[cfg(feature = "transport")]
mod transport;
mod volume;

pub use image::*;
pub use method::*;
#[cfg(feature = "transport")]
pub use transport::*;
pub use volume::*;
