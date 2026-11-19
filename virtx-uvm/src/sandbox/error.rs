use crate::{ImageStoreError, MachineError, NoRoom, VolumeError};

/// What making a sandbox, or asking something of one, fails with.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// What was asked is not a sandbox, or not a command, that could be.
    #[error("{0}")]
    Invalid(String),

    /// An image, a job or anything else asked for that is not there.
    #[error("{0}")]
    NotFound(String),

    /// What this host cannot give, such as a GPU it was built without.
    #[error("{0}")]
    Unsupported(String),

    /// A command the guest did not start, with why.
    #[error("{0}")]
    NotStarted(String),

    /// No room for its machine now, as the embedder's [`Admission`](crate::Admission) says:
    /// nothing was booted.
    #[error(transparent)]
    NoRoom(#[from] NoRoom),

    /// The sandbox's machine has exited, and takes nothing more.
    #[error("the sandbox's machine has exited")]
    Ended,

    /// The job has ended, or never took what was asked of it.
    #[error("{0}")]
    JobEnded(&'static str),

    #[error(transparent)]
    Image(#[from] ImageStoreError),

    /// What it keeps on this host beside its machine's disk -- the lock of its upper disk, its
    /// `/abin` -- that could not be made or read.
    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },

    #[error(transparent)]
    Volume(#[from] VolumeError),

    #[error(transparent)]
    Machine(#[from] MachineError),

    /// Blocking work moved off the runtime that panicked or was cancelled.
    #[error(transparent)]
    Join(#[from] tokio::task::JoinError),
}
