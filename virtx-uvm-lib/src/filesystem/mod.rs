//! Filesystems, in the forms an image's are carried and mounted in.
//!
//! # Terminologies
//!
//! - *changeset*: what one layer of an image is -- files added, replaced and deleted
//!   over whatever is below it.
//! - *archive*: the tar a changeset is carried in to and from a registry, usually
//!   compressed. [`Changeset`] reads and writes them.
//! - *EROFS*: the read-only filesystem a changeset is written as so a guest can mount
//!   it. [`Erofs`] is one.
//! - *ext4*: the writable filesystem a session's changes land on. [`Ext4`] is one.
//! - *diff_id*: the digest of a changeset's archive uncompressed, which is what an image
//!   config signs and so what names the changeset.
//!
//! # A tree in the middle
//!
//! No form is converted into another directly. Each one is read into a `Tree` and written
//! out of one, and the tree is what crosses from one form to the next:
//!
//! ```text
//!   archive ──Changeset::to_tree──┐            ┌──Changeset::from_tree──► archive
//!   EROFS   ──Erofs::to_tree──────┼──► Tree ───┼──Erofs::from_tree──────► EROFS
//!   ext4    ──Ext4::to_tree───────┘            └──Ext4::from_tree───────► ext4
//! ```
//!
//! So pulling a layer is an archive to a tree to an EROFS image, building one is an ext4
//! to a tree to an EROFS image, and handing a session's writes back is an ext4 to a tree
//! to an archive -- and no form has to know about any other. What one form spells in its
//! own way -- a deletion, a directory that hides what is below it, a hard link -- the tree
//! holds as itself, and each form translates it on the way in and back on the way out.
//!
//! Booting an image is several EROFS images to trees, merged into one (`Tree::merge`),
//! and that tree to an EROFS image again -- but one of metadata alone. Its files are read
//! as where their bytes are rather than as the bytes (`erofs::read_spanned`), so what is
//! written points at the layers instead of copying them, and a disk that is the merged
//! image followed by the layers is the image as one filesystem.
//!
//! A tree can also be cut and joined between the two: part of one taken as a tree of its
//! own (`Tree::into_subtree`), one put inside another (`Tree::graft`), a path taken out
//! (`Tree::prune`). That is how a form is read or written somewhere other than its root
//! -- a session's upper layer, which is a directory on its disk -- without any form taking
//! a path to say where.

mod changeset;
pub mod erofs;
mod ext4;
mod tree;

pub use changeset::Changeset;
pub use erofs::Erofs;
pub use ext4::Ext4;
pub use tree::{Data, Entry, Meta, Tree};

/// Re-exported so that nothing outside this module has to name `oci_spec` to hold one.
pub use oci_spec::image::{Digest, MediaType};

/// Which form an [`Error`] is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Form {
    Archive,
    Erofs,
    Ext4,
}

impl std::fmt::Display for Form {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Form::Archive => "archive",
            Form::Erofs => "EROFS image",
            Form::Ext4 => "ext4 image",
        })
    }
}

/// What went wrong, by what would have to change for it not to: the input, this code, the
/// tree, or the host.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// What was read is not what its form says it is: a truncated image, a header that
    /// points outside the file, an archive that names a path outside itself.
    #[error("malformed {form}: {what}")]
    Malformed { form: Form, what: String },

    /// What was read is well-formed, and says something this does not read: a compression,
    /// a block size, an inode layout.
    #[error("unsupported {form}: {what}")]
    Unsupported { form: Form, what: String },

    /// A tree that cannot be written in this form: a name the form cannot spell, an
    /// attribute too large for where it would have to go.
    #[error("cannot be written as an {form}: {what}")]
    Unwritable { form: Form, what: String },

    /// A path a tree was asked to put something at, or to link, that it cannot. Said as
    /// [`Malformed`](Error::Malformed) instead when what asked was a form being read.
    #[error("{0}")]
    Path(String),

    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },
}

impl Error {
    pub(crate) fn io(context: impl Into<String>, source: std::io::Error) -> Error {
        Error::Io {
            context: context.into(),
            source,
        }
    }

    pub(crate) fn malformed(form: Form, what: impl Into<String>) -> Error {
        Error::Malformed {
            form,
            what: what.into(),
        }
    }

    pub(crate) fn unsupported(form: Form, what: impl Into<String>) -> Error {
        Error::Unsupported {
            form,
            what: what.into(),
        }
    }

    pub(crate) fn unwritable(form: Form, what: impl Into<String>) -> Error {
        Error::Unwritable {
            form,
            what: what.into(),
        }
    }

    /// This error, as one about reading `form`: a path the tree would not take is the
    /// form's fault when the form is what named it.
    pub(crate) fn reading(self, form: Form) -> Error {
        match self {
            Error::Path(what) => Error::Malformed { form, what },
            other => other,
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;
