//! A virtx server over HTTP (`virtx-core`'s protocol), as a library: the server its own binary
//! is, to the users of its config's provider, and what a server built on it gives in their place:
//!
//! - [`access::Identity`]: who the tokens are. The binary's is its provider, [`oidc::Oidc`].
//! - [`virtx_uvm::Membership`]: who is a member of which project, in which role. The binary's
//!   is [`access::FileMembership`], each project's members kept in its directory.
//! - [`serve::router`]'s `own`: routes beside the server's. The binary's say where to sign in.

pub mod access;
pub mod config;
pub mod endpoints;
pub mod oidc;
pub mod serve;
