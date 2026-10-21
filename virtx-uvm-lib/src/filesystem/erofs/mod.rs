//! EROFS, the read-only filesystem a layer is so a guest can mount it.
//!
//! Only the format: whiteouts and archives belong to [`Changeset`](super::Changeset).
//! A tree's whiteouts and opaque directories are written here in overlayfs's spelling,
//! which is what the guest reads them with.

mod format;
mod reader;
mod writer;

use std::io::{BufWriter, Read, Seek, Write};
use std::path::Path;
use std::sync::Arc;

use super::tree::Tree;
use super::{Error, Result};

/// What an image is read from: anything that can be read and sought in -- a file, or
/// bytes in memory.
trait Image: Read + Seek + Send {}

impl<T: Read + Seek + Send> Image for T {}

/// An EROFS image, held as what it is read from.
pub struct Erofs<'a> {
    image: Box<dyn Image + 'a>,
}

impl<'a> Erofs<'a> {
    /// `image`, an EROFS image from its byte zero.
    pub fn new(image: impl Read + Seek + Send + 'a) -> Erofs<'a> {
        Erofs {
            image: Box::new(image),
        }
    }

    /// Read the image as a tree, file contents and all.
    pub fn to_tree(self) -> Result<Tree> {
        reader::read(self.image)
    }

    /// Write `tree` onto `out` as an EROFS image, from its byte zero.
    ///
    /// Answers with the files the image points into, in the order they have to follow it:
    /// an [`Span`](super::Data::Span) file is not written, and its inode points
    /// past the image's own end, to where a disk made of the image and then each of those
    /// files, end to end, has its bytes. Nothing, for a tree whose files are all buffers.
    ///
    /// An EROFS inode points at a block, so a file at an offset that is not a whole number
    /// of blocks, or in a file that is not a whole number of them long, is refused.
    pub fn from_tree(tree: &Tree, out: impl Write + Seek) -> Result<Vec<Arc<Path>>> {
        let mut buffered = BufWriter::new(out);
        let behind = writer::write(tree, &mut buffered)?;
        buffered
            .into_inner()
            .map_err(|e| Error::io("writing an EROFS image", e.into_error()))?;
        Ok(behind)
    }
}

/// Read the EROFS image at `path` -- what is in it and where -- and leave the bytes
/// there. Each file is a [`Span`](super::Data::Span) of the image rather than a buffer of
/// its bytes.
///
/// What a merged image is made from (see [`Tree::merge`]): the files stay where they are
/// and the merged image points at them.
///
/// **The image has to be exactly as long as its superblock says**, and one that is not is
/// refused: whatever comes after it on a disk is addressed as if it were, so a file of any
/// other length would put that somewhere nothing points.
///
/// A path and not an [`Erofs`], because an `Erofs` is the image's bytes and not where they
/// are: one read out of memory has no place for a file to point at. Opened here, so the
/// image read is the file every entry names.
pub fn read_spanned(path: impl Into<Arc<Path>>) -> Result<Tree> {
    reader::read_spanned(path.into())
}
