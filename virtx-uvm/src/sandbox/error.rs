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
    /// What was asked cannot be done while the sandbox is in use: a job runs in it, or
    /// something reaches it.
    #[error("{0}")]
    InUse(String),

    /// The sandbox's machine has exited, and takes nothing more.
    #[error("the sandbox's machine has exited")]
    Ended,

    /// The job has ended, or never took what was asked of it.
    #[error("{0}")]
    JobEnded(&'static str),

    /// A write to a job's stdin that its process closed the stdin under: `bytes` of its `of`
    /// went in, and nothing more will.
    #[error("the job's stdin closed after {bytes} of the {of} bytes written to it")]
    StdinCut { bytes: u64, of: u64 },

    /// A write to a job's stdin not taken, `eof` and all: as many writes to it as the guest
    /// holds wait for its process to read. The stdin is as it was.
    #[error(
        "the job's stdin holds as many writes as it takes until its process reads: none of this one was taken"
    )]
    StdinFull,

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

    /// What the storage backend refused, or failed with.
    #[error(transparent)]
    Backend(#[from] crate::backend::Refusal),

    #[error(transparent)]
    Machine(#[from] MachineError),

    /// Blocking work moved off the runtime that panicked or was cancelled.
    #[error(transparent)]
    Join(#[from] tokio::task::JoinError),
}
