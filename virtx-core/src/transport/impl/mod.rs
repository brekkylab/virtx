//! The transports a client and server exchange messages over: stdio and Streamable HTTP.
//!
//! Each one is an [`Outgoing`] and an [`Incoming`] for the client and a way to serve a
//! [`Dispatcher`](super::Dispatcher) for the server.

pub mod stdio;
pub mod streamable_http;

use std::{future::Future, io, pin::Pin};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Where a client's messages go, and how its session ends.
pub trait Outgoing: Send {
    /// Send `message`, one JSON-RPC message. An `Err` means the server is gone.
    fn send(&mut self, message: String) -> BoxFuture<'_, io::Result<()>>;

    /// End the session: nothing is sent after this.
    fn close(self: Box<Self>) -> BoxFuture<'static, ()>;
}

/// Where the server's messages to a client come from.
pub trait Incoming: Send {
    /// The next message. An `Err` ends the session.
    fn receive(&mut self) -> BoxFuture<'_, io::Result<String>>;
}
