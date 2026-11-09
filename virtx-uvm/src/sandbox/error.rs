use crate::{ImageStoreError, MachineError, SnapshotError, VolumeError};

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

    /// A name, an id or a port another has already.
    #[error("{0}")]
    Exists(String),

    /// A command the guest did not start, with why.
    #[error("{0}")]
    NotStarted(String),

    /// The sandbox's machine has exited, and takes nothing more.
    #[error("the sandbox's machine has exited")]
    Ended,

    /// The job has ended, or never took what was asked of it.
    #[error("{0}")]
    JobEnded(&'static str),

    /// What is asked of a sandbox while it runs, and it is stopped.
    #[error("{0}")]
    Stopped(String),

    /// What is asked of a sandbox while it is stopped, and it runs.
    #[error("{0}")]
    Running(String),

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

    #[error(transparent)]
    Image(#[from] ImageStoreError),

    #[error(transparent)]
    Snapshot(#[from] SnapshotError),

    #[error(transparent)]
    Volume(#[from] VolumeError),

    #[error(transparent)]
    Machine(#[from] MachineError),

    #[error(transparent)]
    Build(#[from] crate::BuildError),

    /// Blocking work moved off the runtime that panicked or was cancelled.
    #[error(transparent)]
    Join(#[from] tokio::task::JoinError),
}
