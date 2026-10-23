//! The transports a client and server exchange messages over: stdio and Streamable HTTP.
//!
//! Each one is an [`Outgoing`] and an [`Incoming`] for the client and a way to serve a
//! [`Dispatcher`](super::Dispatcher) for the server. [`Transport`] is what a client holds of
//! one besides those: what it takes to end the session.

pub mod stdio;
pub mod streamable_http;

use std::{future::Future, io, pin::Pin, sync::Arc};

use tokio::process::{Child, Command};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Where a client's messages go. Dropping it is the client's end of the session over stdio.
pub trait Outgoing: Send {
    /// Send `message`, one JSON-RPC message. An `Err` means the server is gone.
    fn send(&mut self, message: String) -> BoxFuture<'_, io::Result<()>>;
}

/// Where the server's messages to a client come from.
pub trait Incoming: Send {
    /// The next message. An `Err` ends the session.
    fn receive(&mut self) -> BoxFuture<'_, io::Result<String>>;
}

/// What a client's session runs over, and what ending it takes.
pub enum Transport {
    /// The server's process, its stdin and stdout the client's.
    Stdio(Child),
    /// The server's endpoint, and the session open there.
    StreamableHttp(Arc<streamable_http::Endpoint>),
}

impl Transport {
    /// Start `command`, a [`std::process::Command`] or a tokio one, with its stdin and stdout
    /// piped to the client: whatever `command` set for those two is replaced. The rest of it
    /// (arguments, environment, directory, stderr) is the caller's. Must be called inside a
    /// tokio runtime.
    pub fn spawn(
        command: impl Into<Command>,
    ) -> io::Result<(impl Outgoing + 'static, impl Incoming + 'static, Transport)> {
        let (sender, receiver, child) = stdio::spawn(command.into())?;
        Ok((sender, receiver, Transport::Stdio(child)))
    }

    /// Reach the endpoint at `url`, an `http` or `https` URL. Nothing is sent until the
    /// client's first call, which opens the session.
    pub fn connect(
        url: &str,
    ) -> anyhow::Result<(impl Outgoing + 'static, impl Incoming + 'static, Transport)> {
        let (sender, receiver, endpoint) = streamable_http::connect(url)?;
        Ok((sender, receiver, Transport::StreamableHttp(endpoint)))
    }

    /// End the session, once the client over it is dropped. Over stdio, the dropped client has
    /// closed the server's stdin: wait for the server to exit, and kill it if it has not
    /// within [`stdio::EXIT_GRACE`]. Over Streamable HTTP, `DELETE` the session if one was
    /// opened.
    pub(crate) async fn close(self) {
        match self {
            Transport::Stdio(child) => stdio::close(child).await,
            Transport::StreamableHttp(endpoint) => streamable_http::close(&endpoint).await,
        }
    }
}
