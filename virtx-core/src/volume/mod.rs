//! What the `volume.file.*` and `volume.mount.*` methods say about the files in a volume.
//!
//! A path in a volume is relative to its root, `/`-separated, as `dir/file.txt`; the empty
//! path is the root. The server refuses one that leads outside the volume.
//!
//! A volume holds files and directories, and nothing else is seen through these methods: a
//! symbolic link in one (as a VM that mounted it may make) is left out of `volume.file.read_dir`,
//! and a path to or through one is refused as if nothing were there.

mod metadata;
mod mount;

pub use metadata::*;
pub use mount::*;
