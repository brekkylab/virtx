//! The Streamable HTTP transport: every message a `POST` to one endpoint, answered with one
//! CBOR message, or with a stream of [`frame`](super::frame)s when the server has
//! notifications to send first.
//!
//! [`Server`] is the server's end; the client's is
//! [`connect`].
//! Both keep to the "Streamable HTTP" section of `PROTOCOL.md`.

#[cfg(feature = "client")]
mod client;
#[cfg(feature = "server")]
mod server;

#[cfg(feature = "client")]
pub use client::{Receiver, Sender, connect};
#[cfg(feature = "server")]
pub use server::{HttpBody, Server};

/// The media type of one message.
const CBOR: &str = "application/cbor";

/// The media type of a stream of messages, each a frame.
const CBOR_SEQ: &str = "application/cbor-seq";

/// Whether `content_type` names `media_type`, whatever its parameters.
fn is_media_type(content_type: &str, media_type: &str) -> bool {
    let name = content_type.split(';').next().unwrap_or("").trim();
    name.eq_ignore_ascii_case(media_type)
}

#[cfg(all(test, feature = "client", feature = "server"))]
mod tests;
