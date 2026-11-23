use crate::{fstree, volume};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// An upper disk that could not be formatted.
    #[error(transparent)]
    Fstree(#[from] fstree::Error),

    /// A volume that could not be opened or held for this machine, or a store in it that
    /// could not be made.
    #[error(transparent)]
    Volume(#[from] volume::Error),

    /// A volume the guest could not be shown where it was asked: somewhere not absolute, the
    /// root, or where another volume already is. Named, not by its path on this host.
    #[error("volume {volume} at {at}: {why}")]
    MountPoint {
        volume: String,
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
    /// what the guest's console and libkrun said. `told` is why without what can name this
    /// host's paths, for a caller elsewhere: the console's lines among those, and why as the
    /// process told it without a path, in place of libkrun's records and this crate's.
    #[error("{account}")]
    DidNotStart { account: String, told: String },

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

    /// What `self` says without this host's paths, for a caller elsewhere: a failure on this
    /// host's files by its kind, a libkrun error by its kind where libkrun's text for it is its
    /// own, and the rest as it says itself. Asked of what [`enter`](super::enter) fails with,
    /// whose contexts name no path.
    pub(super) fn told(&self) -> String {
        match self {
            Error::Io { context, source } => format!("{context}: {}", source.kind()),
            Error::Fstree(fstree::Error::Io { source, .. })
            | Error::Volume(volume::Error::Io { source, .. }) => {
                format!("the server's files: {}", source.kind())
            }
            Error::Vmm { context, source } => format!("{context}: {}", vmm_told(source)),
            Error::Init(krun_init::ApplyError::SymbolNotFound(_)) => self.to_string(),
            Error::Init(krun_init::ApplyError::OverlayError(source)) => {
                format!(
                    "preparing the guest init: overlay error: {}",
                    vmm_told(source)
                )
            }
            Error::Init(_) => "preparing the guest init: a failure in libkrun".into(),
            _ => self.to_string(),
        }
    }
}

/// `e` by its kind: as it says itself where libkrun's text for it is fixed, and its kind
/// alone where the text is libkrun's own, which can name this host's paths.
fn vmm_told(e: &krun::VmmError) -> String {
    use krun::VmmError;
    match e {
        VmmError::InvalidParam()
        | VmmError::OutOfRange()
        | VmmError::FileNotFound()
        | VmmError::ResourceAlloc()
        | VmmError::BadFd()
        | VmmError::FeatureDisabled() => e.to_string(),
        VmmError::MissingConfig(_) => "missing required configuration".into(),
        VmmError::BootError(_) => "boot error".into(),
        VmmError::Internal(_) => "internal error".into(),
        _ => "a failure in libkrun".into(),
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use std::io;

    use super::*;

    #[test]
    fn what_a_machine_s_process_tells_names_none_of_this_host_s_paths() {
        const PATH: &str = "/home/virtx/project/a/volume/v";
        let named = [
            (
                Error::vmm(
                    "attaching the root's image",
                    krun::VmmError::BootError(format!("opening {PATH}/a.vmdk")),
                ),
                "attaching the root's image: boot error",
            ),
            (
                Error::io(
                    "reading the machine to be",
                    io::Error::other(format!("{{\"rootfs\": \"{PATH}\"}}")),
                ),
                "reading the machine to be: other error",
            ),
            (
                Error::Volume(volume::Error::Io {
                    context: format!("reading {PATH}/mounts.json"),
                    source: io::ErrorKind::InvalidData.into(),
                }),
                "the server's files: invalid data",
            ),
            (
                Error::Init(krun_init::ApplyError::OverlayError(
                    krun::VmmError::Internal(PATH.into()),
                )),
                "preparing the guest init: overlay error: internal error",
            ),
        ];
        for (e, told) in named {
            assert!(e.to_string().contains(PATH), "{e}");
            assert_eq!(e.told(), told);
        }

        let fixed = [
            Error::vmm("sharing /abin", krun::VmmError::FileNotFound()),
            Error::MountPoint {
                volume: "v".into(),
                at: "/".into(),
                why: "/ is the guest's root",
            },
        ];
        for e in fixed {
            assert_eq!(e.told(), e.to_string());
        }
    }
}
