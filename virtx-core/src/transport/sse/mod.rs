//! A job's log, which the protocol's Events has a server send as an event stream: each entry
//! numbered, read from its start or from after the entry a client last saw. The client's end is
//! [`client`], the server's [`server`].

#[cfg(feature = "client")]
pub(crate) mod client;
#[cfg(feature = "server")]
pub(crate) mod server;
