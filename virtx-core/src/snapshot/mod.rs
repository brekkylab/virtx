//! Snapshots: the upper layer of a VM's root filesystem, kept by the server under a name, made
//! and removed at `/snapshots/`. Its files are reached at `/snapshots/{name}/files/` with
//! WebDAV.

#[cfg(feature = "client")]
pub(crate) mod client;
mod store;

pub use store::*;
