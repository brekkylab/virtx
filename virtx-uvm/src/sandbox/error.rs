use crate::{ImageStoreError, MachineError, NoRoom, SnapshotError, VolumeError};

/// What making a sandbox, or asking something of one, fails with.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// What was asked is not a sandbox, or not a command, that could be.
    #[error("{0}")]
    Invalid(String),

    /// A sandbox, an image, a job or anything else asked for that is not there.
    #[error("{0}")]
    NotFound(String),

    /// What this host cannot give, such as a GPU it was built without.
    #[error("{0}")]
    Unsupported(String),

    /// A name or an id another has already.
    #[error("{0}")]
    Exists(String),

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

    /// What is asked of a sandbox while nothing uses it, and something does: a job runs in
    /// it, or it is reached through its files or its ports.
    #[error("{0}")]
    InUse(String),

    /// Refused by the server's [`Hooks`](super::Hooks), as it says: an HTTP status, and the
    /// `code` and `message` of its body.
    #[error("{message}")]
    Refused {
        status: u16,
        code: &'static str,
        message: String,
    },

    /// Something on this host that should not have failed.
    #[error("{0}")]
    Failed(String),
    /// A write to a job's stdin that its process closed the stdin under: `bytes` of its `of`
    /// went in, and nothing more will.
    #[error("the job's stdin closed after {bytes} of the {of} bytes written to it")]
    StdinCut { bytes: u64, of: u64 },

    #[error(transparent)]
    Image(#[from] ImageStoreError),

    #[error(transparent)]
    Snapshot(#[from] SnapshotError),

    #[error(transparent)]
    Volume(#[from] VolumeError),

    /// What the storage backend refused, or failed with.
    #[error(transparent)]
    Backend(#[from] crate::backend::Refusal),

    #[error(transparent)]
    Machine(#[from] MachineError),

    #[error(transparent)]
    Build(#[from] crate::BuildError),

    /// Blocking work moved off the runtime that panicked or was cancelled.
    #[error(transparent)]
    Join(#[from] tokio::task::JoinError),
}
