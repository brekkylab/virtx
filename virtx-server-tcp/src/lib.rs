//! A virtx server over HTTP (`virtx-core`'s protocol), as a library: the server its own binary
//! is, to the projects its config names, and what a server built on it gives in their place:
//!
//! - [`access::Tenants`]: whose the tokens are, and the homes they reach. The binary's are
//!   [`access::Projects`].
//! - [`endpoints::Images`]: what answers a home's registry. The binary's is a
//!   [`virtx_uvm::RegistryServer`].
//! - [`serve::router`]'s `own`: routes beside the server's. The binary's say where to sign in.

pub mod access;
pub mod config;
pub mod endpoints;
pub mod oidc;
pub mod serve;
