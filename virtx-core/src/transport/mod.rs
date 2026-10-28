//! The RPC endpoint of `PROTOCOL.md`: JSON-RPC 2.0, each message a `POST` to `/rpc`, answered
//! with one JSON message or with an event stream of the request's notifications and then its
//! response.
//!
//! Both transports carry HTTP: over TCP, or as one HTTP/2 connection over the server's stdin
//! and stdout. The client end is the `client` feature, the server end the `server` feature.

#[cfg(feature = "client")]
mod client;
mod message;
#[cfg(feature = "server")]
mod server;
#[cfg(all(test, feature = "client", feature = "server"))]
pub(crate) mod tests;

#[cfg(feature = "client")]
pub use client::{Error, PortForwarder, TransportClient};
pub use message::ErrorObject;
#[cfg(feature = "server")]
pub use server::{CancelScope, Dispatcher, HttpBody, Methods, Notifier, Server, serve_stdio};
