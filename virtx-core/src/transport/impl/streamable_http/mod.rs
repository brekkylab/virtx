//! The Streamable HTTP transport: every message a `POST` to one endpoint, answered with JSON,
//! or with an SSE stream when the server has notifications to send first.
//!
//! [`Server`] is the server's end; the client's is
//! [`Transport::connect`](crate::transport::Transport::connect).
//! Both keep to the "Streamable HTTP" section of `PROTOCOL.md`, including its session header.

mod client;
mod server;
mod sse;

pub use client::Endpoint;
pub(crate) use client::{close, connect};
pub use server::{Server, SessionId};

/// The header a session's id travels in, both ways.
pub const SESSION_HEADER: &str = "virtx-session-id";

/// The media type of an SSE stream.
const EVENT_STREAM: &str = "text/event-stream";

#[cfg(test)]
mod tests;
