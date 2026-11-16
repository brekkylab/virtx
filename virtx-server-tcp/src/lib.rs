//! A virtx server over HTTP (`virtx-core`'s protocol), as a library: the server its own binary
//! is, to the projects its config names, and the places a server built on it plugs into --
//! virtx cloud's node, behind its gateway:
//!
//! - [`access::Authenticator`]: who the tokens are, and the projects they reach. The binary's is
//!   [`access::Projects`].
//! - [`access::Tenants`]: in place of an authenticator, who the tokens are and the homes they
//!   reach, which they make.
//! - [`keeping::Keeping`]: where a home's snapshots, volumes and images are kept past it.
//! - [`serve::router`]'s `own`: routes of its own, beside the server's, reading
//!   [`access::Access`] and [`endpoints::Home`]. The binary's say where to sign in.

pub mod access;
pub mod config;
pub mod endpoints;
pub mod keeping;
pub mod oidc;
pub mod serve;
pub mod terminals;
