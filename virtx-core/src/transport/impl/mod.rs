//! The transports a client and server exchange messages over: stdio and Streamable HTTP.
//!
//! Each one is an `Outgoing` and an `Incoming` for the client (the `client` feature) and a way
//! to serve a `Dispatcher` for the server (the `server` feature).

pub(crate) mod frame;
pub mod stdio;
pub mod streamable_http;

#[cfg(feature = "client")]
use std::io;
#[cfg(any(feature = "client", feature = "server"))]
use std::{future::Future, pin::Pin};

#[cfg(any(feature = "client", feature = "server"))]
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[cfg(feature = "client")]
/// Where a client's messages go, and how its session ends.
pub trait Outgoing: Send {
    /// Send `message`, one JSON-RPC message as CBOR. An `Err` means the server is gone.
    fn send(&mut self, message: Vec<u8>) -> BoxFuture<'_, io::Result<()>>;

    /// End the session: nothing is sent after this.
    fn close(self: Box<Self>) -> BoxFuture<'static, ()>;
}

#[cfg(feature = "client")]
/// Where the server's messages to a client come from.
pub trait Incoming: Send {
    /// The next message. An `Err` ends the session.
    fn receive(&mut self) -> BoxFuture<'_, io::Result<Vec<u8>>>;
}
