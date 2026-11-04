use std::path::PathBuf;

use crate::{fstree, snapshot, volume};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A temporary snapshot that could not be formatted.
    #[error(transparent)]
    Fstree(#[from] fstree::Error),

    /// The snapshot could not be held for this machine: another one runs over it.
    #[error(transparent)]
    Snapshot(#[from] snapshot::Error),

    /// A volume that could not be opened or held for this machine, or a store in it that
    /// could not be made.
    #[error(transparent)]
    Volume(#[from] volume::Error),

    /// A volume the guest could not be shown where it was asked: somewhere not absolute, the
    /// root, or where another volume already is.
    #[error("volume {} at {at}: {why}", path.display())]
    MountPoint {
        path: PathBuf,
        at: String,
        why: &'static str,
    },

    /// A port that cannot be published: `0`, or one given twice.
    #[error("port {port}: {why}")]
    Port { port: u16, why: &'static str },

    /// A device, a payload or a machine libkrun would not build. What went wrong is
    /// libkrun's to say; what it was doing is this end's.
    #[error("{context}: {source}")]
    Vmm {
        context: String,
        #[source]
        source: krun::VmmError,
    },

    /// The guest init could not be laid over the root, which is a machine with no PID 1.
    #[error("preparing the guest init: {0}")]
    Init(#[source] krun_init::ApplyError),

    /// The machine started and its event loop then broke -- the only way [`enter`] comes back
    /// once the machine is built.
    ///
    /// No source, because there is none to carry: libkrun logs what went wrong and returns
    /// nothing.
    #[error("the machine's event loop stopped; libkrun logged why")]
    EventLoop,

    /// A machine given no command, which would have nothing to run.
    #[error("a machine runs a command, and none was given")]
    NoCommand,

    /// The machine's process could not build the machine, or it broke: why, with the end of
    /// what the guest's console and libkrun said.
    #[error("{0}")]
    DidNotStart(String),

    /// A command the guest could not start, and why.
    #[error("{0}")]
    NotStarted(String),
    /// The machine's process ended without a command's exit code -- killed, or crashed: how,
    /// with the end of what the guest's console and libkrun said.
    #[error("{0}")]
    Died(String),

    #[error("the command ran past its timeout of {0}ms and was killed")]
    TimedOut(u64),

    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },
}

impl Error {
    pub fn vmm(context: impl Into<String>, source: krun::VmmError) -> Error {
        Error::Vmm {
            context: context.into(),
            source,
        }
    }

    pub fn io(context: impl Into<String>, source: std::io::Error) -> Error {
        Error::Io {
            context: context.into(),
            source,
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;
