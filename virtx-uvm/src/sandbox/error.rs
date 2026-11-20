use crate::{ImageStoreError, MachineError, NoRoom, VolumeError, fstree};

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

impl Error {
    /// What `self` says without this host's paths, for a caller elsewhere: a failure on this
    /// host's files by its kind, a machine that did not start as its process told it, and the
    /// rest as it says itself.
    pub(super) fn told(&self) -> String {
        match self {
            Error::Io { source, .. }
            | Error::Image(ImageStoreError::Io { source, .. })
            | Error::Image(ImageStoreError::Fstree(fstree::Error::Io { source, .. }))
            | Error::Volume(VolumeError::Io { source, .. })
            | Error::Machine(MachineError::Io { source, .. })
            | Error::Machine(MachineError::Fstree(fstree::Error::Io { source, .. }))
            | Error::Machine(MachineError::Volume(VolumeError::Io { source, .. })) => {
                format!("the server's files: {}", source.kind())
            }
            Error::Machine(MachineError::DidNotStart { told, .. }) => told.clone(),
            _ => self.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io;

    use super::*;

    #[test]
    fn what_a_sandbox_s_failure_tells_names_none_of_this_host_s_paths() {
        const PATH: &str = "/home/virtx/project/a/sandbox/s.json";
        let context = || format!("reading {PATH}");
        let source = || io::Error::from(io::ErrorKind::InvalidData);
        let tree = || fstree::Error::Io {
            context: context(),
            source: source(),
        };
        let volume = || VolumeError::Io {
            context: context(),
            source: source(),
        };
        let on_files = [
            Error::Io {
                context: context(),
                source: source(),
            },
            Error::Image(ImageStoreError::Io {
                context: context(),
                source: source(),
            }),
            Error::Image(ImageStoreError::Fstree(tree())),
            Error::Volume(volume()),
            Error::Machine(MachineError::io(context(), source())),
            Error::Machine(MachineError::Fstree(tree())),
            Error::Machine(MachineError::Volume(volume())),
        ];
        for e in on_files {
            assert!(e.to_string().contains(PATH), "{e}");
            assert_eq!(e.told(), "the server's files: invalid data");
        }

        let told = "the machine's process ended with exit status: 1 before it was up";
        let did_not_start = Error::Machine(MachineError::DidNotStart {
            account: format!("{told}; last logged:\nvirtx_uvm::machine::machine: {PATH}"),
            told: told.into(),
        });
        assert_eq!(did_not_start.told(), told);

        let gone = Error::NotFound(format!("no image here is sha256:{}", "0".repeat(64)));
        assert_eq!(gone.told(), gone.to_string());
    }
}
