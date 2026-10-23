//! The Streamable HTTP transport: every message a `POST` to one endpoint, answered with one
//! CBOR message, or with a stream of [`frame`](super::frame)s when the server has
//! notifications to send first.
//!
//! [`Server`] is the server's end; the client's is
//! [`connect`].
//! Both keep to the "Streamable HTTP" section of `PROTOCOL.md`, including its session header.

mod client;
mod server;

pub use client::{Receiver, Sender, connect};
pub use server::{HttpBody, Server, SessionId};

/// The header a session's id travels in, both ways.
pub const SESSION_HEADER: &str = "virtx-session-id";

/// The media type of one message.
const CBOR: &str = "application/cbor";

/// The media type of a stream of messages, each a frame.
const CBOR_SEQ: &str = "application/cbor-seq";

/// Whether `content_type` names `media_type`, whatever its parameters.
fn is_media_type(content_type: &str, media_type: &str) -> bool {
    let name = content_type.split(';').next().unwrap_or("").trim();
    name.eq_ignore_ascii_case(media_type)
}

#[cfg(test)]
mod tests;
