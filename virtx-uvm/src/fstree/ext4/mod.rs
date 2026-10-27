//! ext4, the filesystem a session's writable upper layer is.
//!
//! Read and written in overlayfs's spelling, which is how the guest left it and how it
//! will read it: a deletion is a character device numbered 0:0, and an opaque directory
//! carries `trusted.overlay.opaque`. Everything else overlayfs writes under
//! `trusted.overlay.` -- `origin`, `impure`, `nlink` and the rest -- is read as it is, so
//! an image read and written back is the image it was; a tree going on to be a layer has
//! it taken out ([`FsTree::strip_overlay`]).
//!
//! That holds only for an upper written without `redirect_dir` or `metacopy`, which keep
//! part of what changed on the lower: a renamed directory's contents, a file whose data was
//! never copied up. Read alone, such an upper is not the whole of a session's work. The
//! guest's init mounts its overlay without either.

mod reader;
mod writer;

use std::collections::{HashMap, HashSet};
use std::io::{BufWriter, Read, Seek, Write};

use self::reader::{ROOT, Reader, Walk};
use self::writer::Plan;
use super::{Error, FsTree, Result};

impl FsTree {
    /// The tree in `image`, an ext4 image from its byte zero, file contents and all.
    ///
    /// **The filesystem has to have been left clean**, and one that was not is refused: a
    /// guest killed with writes still in its page cache has not put them in the image, and
    /// one whose kernel found the filesystem inconsistent has left something nothing here
    /// could make right. Saying it was left clean, once the machine is down and flushed, is
    /// the caller's.
    pub fn from_ext4(image: impl Read + Seek) -> Result<FsTree> {
        let mut reader = Reader::open(image)?;
        let root = reader.inode(ROOT)?;
        if root.mode & 0o170000 != 0o040000 {
            return Err(Error::Malformed(format!(
                "ext4 image: the root inode is mode {:o}, which is not a directory",
                root.mode
            )));
        }

        let mut walk = Walk {
            tree: FsTree::new(),
            files: HashMap::new(),
            open: HashSet::new(),
        };
        walk.directory(&mut reader, ROOT, root, Vec::new(), 0)?;
        Ok(walk.tree)
    }

    /// Write the tree onto `out` as a new ext4 image of `size` bytes, from its byte zero.
    ///
    /// `size` bounds what the guest may write and not what the image costs: nothing is
    /// written where the filesystem is empty, so a file `out` is stays as sparse as the
    /// host keeps it. For the same reason `out` has to read as zeros wherever nothing is
    /// written -- a new file, or an empty buffer.
    pub fn to_ext4(&self, out: impl Write + Seek, size: u64) -> Result<()> {
        let plan = Plan::new(self, size)?;
        let mut buffered = BufWriter::new(out);
        plan.emit(&mut buffered)
            .map_err(|e| Error::io("writing an ext4 image", e))?;
        buffered
            .into_inner()
            .map_err(|e| Error::io("writing an ext4 image", e.into_error()))?;
        Ok(())
    }
}
