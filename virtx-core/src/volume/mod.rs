//! Volumes, and the remote sources mounted in them. A volume's files are reached at
//! `/volumes/{name}/` with WebDAV, not through these methods.
//!
//! A path in a volume is relative to its root, `/`-separated, as `dir/file.txt`. The server
//! refuses one that leads outside the volume.

mod mount;
mod store;

pub use mount::{MountSource, MountType};
pub use store::*;
