//! The stdio transport: messages as [`frame`](super::frame)s on the server's stdin and stdout.
//!
//! [`spawn`] is the client's end; the server's is [`serve`].

#[cfg(feature = "client")]
mod client;
#[cfg(feature = "server")]
mod server;

#[cfg(feature = "client")]
pub use client::{EXIT_GRACE, Receiver, Sender, spawn};
#[cfg(feature = "server")]
pub use server::serve;

#[cfg(all(test, feature = "client", feature = "server"))]
mod tests;
