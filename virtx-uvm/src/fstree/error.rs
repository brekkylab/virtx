/// What went wrong, by what would have to change for it not to: the input, this code, the
/// tree, or the host.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// What was read is not what its form says it is: a truncated image, a header that
    /// points outside the file, an archive that names a path outside itself.
    #[error("malformed {0}")]
    Malformed(String),

    /// What was read is well-formed, and says something this does not read: a compression,
    /// a block size, an inode layout.
    #[error("unsupported {0}")]
    Unsupported(String),

    /// A tree that cannot be written in this form: a name the form cannot spell, an
    /// attribute too large for where it would have to go.
    #[error("cannot be written as an {0}")]
    Unwritable(String),

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

    /// This error, as one about reading a form: a path the tree would not take is the
    /// form's fault when the form is what named it, and `malformed` says it as that form's.
    pub(crate) fn reading(self, malformed: fn(String) -> Error) -> Error {
        match self {
            Error::Path(what) => malformed(what),
            other => other,
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;
