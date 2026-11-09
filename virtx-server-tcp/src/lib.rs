//! A virtx server over HTTP (`virtx-core`'s protocol), as a library: the server its own binary
//! is, to the users and projects its config names, and the places a server built on it plugs
//! into -- virtx cloud's node, behind its gateway:
//!
//! - [`access::Tenants`]: who the tokens no configured user has are, and the homes they reach.
//! - [`keeping::Keeping`]: where a home's snapshots, volumes and images are kept past it.
//! - [`serve::router`]'s `own`: routes of its own, beside the server's, reading
//!   [`access::Access`] and [`endpoints::Home`].

pub mod access;
pub mod config;
pub mod endpoints;
pub mod keeping;
pub mod serve;
pub mod terminals;
