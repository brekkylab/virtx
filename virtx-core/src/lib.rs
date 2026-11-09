//! A virtx client behind a C ABI.
//!
//! The client is an opaque handle: C holds only the pointer `virtx_client_spawn` or
//! `virtx_client_connect` writes, shared by `virtx_client_retain` and `virtx_client_release`. What
//! follows is the same over every transport, so only those two name one. `include/virtx.h`
//! declares these functions, and changes with them.
//!
//! The types every request takes and answers are always built; the wire's client end, and
//! [`Client`] over it, is the `client` feature, its server end the `server` feature, and the C
//! ABI over a client the `ffi` feature, on by default with `client`. The `virtx` command,
//! [`cli`], is the `cli` feature, also on by default so that `libvirtx_core` carries it. A crate
//! that reaches the library through the C ABI takes the types alone, with
//! `default-features = false`; a server takes `server`.

mod auth;
#[cfg(feature = "cli")]
pub mod cli;
#[cfg(feature = "client")]
mod client;
#[cfg(feature = "client")]
mod ensure;
#[cfg(feature = "client")]
mod error;
#[cfg(feature = "ffi")]
mod ffi;
mod image;
mod sandbox;
#[cfg(feature = "testing")]
pub mod testing;
mod transport;
mod volume;

pub use auth::Auth;
#[cfg(feature = "client")]
pub use client::Client;
#[cfg(feature = "client")]
pub use ensure::{cache_root, ensure_virtx};
#[cfg(feature = "client")]
pub use error::Error;
pub use image::*;
pub use sandbox::*;
pub use transport::*;
pub use volume::*;
