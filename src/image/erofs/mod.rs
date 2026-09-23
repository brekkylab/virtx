//! EROFS, the read-only filesystem a layer becomes so a guest can mount it.
//!
//! [`write`] puts a [`Tree`](crate::image::tree::Tree) down as an image and [`read`]
//! takes one back out of one. Neither knows anything about OCI -- whiteouts and layer
//! archives belong to [`crate::image::blob`]. What is here is only the format.
//!
//! # Two kinds of image
//!
//! [`write`] writes a layer, which carries its own file data. [`write_meta`] writes
//! metadata alone, for inodes whose data blocks lie elsewhere on the disk the image is
//! the front of -- which is how a stack of layers is read as one filesystem, each
//! inode addressing the layer its bytes are still sitting in. The two differ in one
//! field of the inode and in whether a data area is written at all.
//!
//! That is also why nothing here ever inlines a file's tail. EROFS lets a file's last
//! partial block sit behind its inode, which saves the padding -- but those bytes are
//! then in the metadata a merge replaces, and a merged image cannot point at them.

mod format;
mod reader;
mod writer;

pub use format::BLOCK_SIZE;
pub use reader::read;
pub use writer::{meta_blocks, write, write_meta};

/// What went wrong with an image, on the way in or on the way out.
#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    /// A directory entry whose name cannot fit in a block beside its dirent.
    NameTooLong(Vec<u8>),
    /// An inode whose inline xattrs leave it unable to fit in one block.
    XattrsTooLarge,
    /// The tar held fewer bytes than the tree says a file has, which means the tree
    /// and the archive it was read from have come apart.
    ShortRead,
    /// An image that is not one: a bad magic, a size that does not fit what is there,
    /// a dirent pointing outside its own block.
    Malformed(String),
    /// An image that is one, saying something this reads no images saying. Every
    /// layer under this cache is written by [`write`], so what these name is a file
    /// that came from somewhere else -- or from a version of it that this is not.
    Unsupported(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(e) => write!(f, "{e}"),
            Error::NameTooLong(name) => {
                write!(
                    f,
                    "{} is too long to name a directory entry",
                    String::from_utf8_lossy(name)
                )
            }
            Error::XattrsTooLarge => write!(f, "an inode's xattrs do not fit beside it"),
            Error::ShortRead => write!(f, "the layer archive ended before a file it describes"),
            Error::Malformed(what) => write!(f, "{what}"),
            Error::Unsupported(what) => write!(f, "{what}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(source: std::io::Error) -> Error {
        Error::Io(source)
    }
}

type Result<T> = std::result::Result<T, Error>;

/// What an image came out as, or what one turned out to be.
///
/// `blocks` is the image's own length, whatever its superblock says the disk around
/// it is: a metadata image is an extent of a longer disk, and how long that disk is
/// says nothing about how long the extent is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    pub blocks: u32,
    pub bytes: u64,
    pub inodes: u64,
}
