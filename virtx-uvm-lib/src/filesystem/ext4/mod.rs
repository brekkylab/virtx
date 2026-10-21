//! ext4, the filesystem a session's writable upper layer is.
//!
//! Read and written in overlayfs's spelling, which is how the guest left it and how it
//! will read it: a deletion is a character device numbered 0:0, and an opaque directory
//! carries `trusted.overlay.opaque`. Everything else overlayfs writes under
//! `trusted.overlay.` -- `origin`, `impure`, `nlink` and the rest -- is one mount's
//! bookkeeping about a stack that will not exist again, and is dropped on the way in.
//!
//! That holds only for an upper written without `redirect_dir` or `metacopy`, which keep
//! part of what changed on the lower: a renamed directory's contents, a file whose data was
//! never copied up. Read alone, such an upper is not the whole of a session's work. The
//! guest's init mounts its overlay without either.
//!
//! Where the upper sits on the disk -- which directory is the upperdir -- is the boot's
//! arrangement and not the filesystem's, so nothing here knows it: a tree is read from and
//! written to the disk's root, and the caller takes the part it wants
//! ([`Tree::into_subtree`]) or puts the part it has where it goes ([`Tree::graft`]).

mod reader;
mod writer;

use std::io::{Read, Seek};
use std::path::Path;

use super::Result;
use super::tree::Tree;

/// What an image is read from: anything that can be read and sought in -- a file, or
/// bytes in memory.
trait Image: Read + Seek + Send {}

impl<T: Read + Seek + Send> Image for T {}

/// An ext4 image, held as what it is read from.
pub struct Ext4<'a> {
    image: Box<dyn Image + 'a>,
}

impl<'a> Ext4<'a> {
    /// `image`, an ext4 image from its byte zero.
    pub fn new(image: impl Read + Seek + Send + 'a) -> Ext4<'a> {
        Ext4 {
            image: Box::new(image),
        }
    }

    /// Read the whole filesystem as a tree, file contents and all.
    ///
    /// **The filesystem has to have been left clean**, and one that was not is refused: a
    /// guest killed with writes still in its page cache has not put them in the image, and
    /// one whose kernel found the filesystem inconsistent has left something nothing here
    /// could make right. Saying it was left clean, once the machine is down and flushed, is
    /// the caller's.
    pub fn to_tree(self) -> Result<Tree> {
        reader::read(self.image)
    }

    /// Format a new ext4 image at `path` holding `tree`, `size` bytes being the most it can
    /// go on to hold.
    ///
    /// `size` bounds what the guest may write and not what the image costs: the file is
    /// sparse. The tree's root is the filesystem's root, and keeps what the formatter gives
    /// a root rather than the tree's metadata for it.
    ///
    /// **A new image and never an existing one**, because the formatter writes a filesystem
    /// in one pass and has no way to open one. A path already there is replaced. A failure
    /// leaves no file behind: a half-written filesystem found by the run after would be
    /// mounted as if it were one.
    pub fn from_tree(tree: &Tree, path: &Path, size: u64) -> Result<()> {
        let written = writer::write(tree, path, size);
        if written.is_err() {
            let _ = std::fs::remove_file(path);
        }
        written
    }
}
