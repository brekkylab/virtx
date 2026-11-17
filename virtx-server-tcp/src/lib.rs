//! A virtx server over HTTP (`virtx-core`'s protocol), as a library: the server its own binary
//! is, to the users of its config's provider, and what a server built on it gives in their place:
//!
//! - [`access::Identity`]: who the tokens are. The binary's is its issuer, [`oidc::Oidc`].
//! - [`project::Membership`]: which project an owner names so, and who is a member of each, in
//!   which role. The binary's is [`project::TcpMembership`], each project kept in its
//!   directory.
//! - [`access::Homes`]: what makes a project's home as it is first reached. The binary's is
//!   [`virtx_uvm::ProjectStore`], each home in the project's directory.
//! - [`serve::router`]'s `own`: routes beside the server's. The binary's say where to sign in.

pub mod access;
pub mod config;
pub mod endpoints;
pub mod oidc;
pub mod project;
pub mod serve;
