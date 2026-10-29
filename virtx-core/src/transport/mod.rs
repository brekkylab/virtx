//! How a client reaches a server's endpoints, as `PROTOCOL.md`'s Transport says: HTTP over TCP,
//! or one HTTP/2 connection over the server's stdin and stdout. Each endpoint's protocol, such
//! as [`rpc`](crate::rpc)'s, is built on it and knows no transport.
//!
//! The client end is the `client` feature, the server end the `server` feature.

#[cfg(feature = "client")]
mod client;
#[cfg(feature = "server")]
mod server;

#[cfg(feature = "client")]
pub(crate) use client::{Body, full};
#[cfg(feature = "client")]
pub use client::{PortForwarder, TransportClient};
#[cfg(feature = "server")]
pub use server::serve_stdio;
