//! The RPC endpoint of `PROTOCOL.md`: JSON-RPC 2.0, each message a `POST` to `/rpc`, answered
//! with one JSON message or with an event stream of the request's notifications and then its
//! response.
//!
//! It goes over either [`transport`](crate::transport). The client end is the `client` feature,
//! the server end the `server` feature.

#[cfg(feature = "client")]
mod client;
#[cfg(feature = "server")]
mod dispatch;
mod message;
#[cfg(feature = "server")]
mod server;
#[cfg(all(test, feature = "client", feature = "server"))]
pub(crate) mod tests;

#[cfg(feature = "server")]
pub use dispatch::{CancelScope, Dispatcher, Methods, Notifier};
pub use message::ErrorObject;
#[cfg(feature = "server")]
pub use server::{HttpBody, Server};
