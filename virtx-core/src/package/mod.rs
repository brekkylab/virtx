//! Packages: a sandbox kept under a name at `/packages/`, made as many times as it is asked for
//! at `/packages/{package}/sandboxes/`, by the project's members and by whom its access lets in,
//! each of those reaching the sandbox it made alone with its grant.

#[cfg(feature = "client")]
pub(crate) mod client;
mod store;

pub use store::*;
