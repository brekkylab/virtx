//! Volumes, made and removed at `/volumes/`, and the remote sources mounted in them at
//! `/volumes/{name}/mounts/`. A volume's files are reached at `/volumes/{name}/files/` with
//! WebDAV.
//!
//! A path in a volume is relative to its root, `/`-separated, as `dir/file.txt`. The server
//! refuses one that leads outside the volume.

#[cfg(feature = "client")]
pub(crate) mod client;
mod mount;
mod store;

pub use mount::{MountSource, MountType};
pub use store::*;
