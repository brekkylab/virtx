//! Running an image: the writable layer a session gets, and the machine that runs on it.
//!
//! # What a session is
//!
//! One boot of one image, with one file to show for it. [`image`](crate::image) answers
//! with a [`Disk`](crate::image::Disk) -- the layers stitched into a single read-only
//! EROFS filesystem -- and that disk is shared by every session ever started from the
//! image. What makes a session its own is its [`UpperLayer`]: an ext4 image the guest
//! stacks over that disk, holding every byte it writes and nothing anyone else wrote.
//!
//! ```text
//!   /dev/vdb  ext4,  writable    this session's upper layer ─┐
//!                                                            ├─ overlay ──► /
//!   /dev/vda  erofs, read-only   the image's disk           ─┘
//! ```
//!
//! So a session is thrown away by deleting one file, and carried on by keeping it: a
//! second [`enter`] over the same upper layer starts where the first one stopped, because
//! the overlay's upperdir is a directory on a disk that outlived the machine.
//!
//! # Where the stacking happens
//!
//! In the guest, by libkrun's init, which this repo patches to take two devices where it
//! took one -- see `patches/libkrun-85bed71-overlay-root.patch`. It has to be the guest:
//! overlayfs is a kernel filesystem and the host has no way to mount one for it, let
//! alone an EROFS the host kernel may not even read.
//!
//! Two block devices rather than a writable virtio-fs root, because overlayfs cannot copy
//! up from a virtio-fs lower -- and without copy-up, editing any file the image shipped
//! fails.
//!
//! # What a run does not come back from
//!
//! [`enter`] enters the VMM, and the VMM exits the process when the guest goes down. So
//! it returns only by failing, which is what the
//! [`Infallible`](std::convert::Infallible) in its signature says: everything after it in
//! a caller is unreachable. That is also why there is one operation here and not two --
//! nothing can run after a session starts, so nothing can ask a second thing of it.

mod ops;
mod upper_layer;

use std::path::PathBuf;

pub use ops::enter;
pub use upper_layer::UpperLayer;

/// What a session is, beyond the image it runs: where its writes go, how big a machine
/// runs them, and what it runs.
///
/// [`at`](Session::at) gives every field a working default but the one that cannot have
/// one -- the upper layer, which *is* the session. Anything else is changed by naming
/// it:
///
/// ```no_run
/// # use cortex_krun::session::Session;
/// let session = Session {
///     command: vec!["/bin/sh".to_string(), "-c".to_string(), "echo hi".to_string()],
///     ram_mib: 4096,
///     ..Session::at("/tmp/demo.ext4")
/// };
/// ```
#[derive(Clone, Debug)]
pub struct Session {
    /// The ext4 image this session writes to, created if it is not there. Keeping it is
    /// what carries a session on; deleting it is what throws one away. See [`UpperLayer`].
    pub upper_layer: PathBuf,

    /// A ceiling on how much the session may write, and **not** an allocation: the file
    /// is sparse, so a session that writes a kilobyte occupies a kilobyte. Two gibibytes
    /// by default, and ignored for an upper layer that is already there -- see
    /// [`UpperLayer::at`].
    pub size: u64,

    pub vcpus: u8,
    pub ram_mib: u32,

    /// What to run, in place of what the image states as its `Cmd`.
    ///
    /// **The image's `Entrypoint` still goes in front of it**, which is what docker does
    /// with the same words: an image whose entrypoint is `/usr/bin/tini --` runs its
    /// entrypoint whatever it is asked for. Empty is the image's own `Cmd`, and an image
    /// that states neither runs `/bin/sh`.
    pub command: Vec<String>,

    /// Environment on top of the image's, each `KEY=value`. A name the image already set
    /// is set twice and the later one wins, so this overrides rather than merges.
    pub env: Vec<String>,
}

impl Session {
    /// A session writing to `upper_layer`, with a default machine around it.
    pub fn at(upper_layer: impl Into<PathBuf>) -> Session {
        Session {
            upper_layer: upper_layer.into(),
            size: upper_layer::SIZE,
            // Two, because a command and whatever is watching it are two things wanting
            // the processor at once, and one vCPU turns that into a queue.
            vcpus: 2,
            // Enough for a package install, which is the first thing anyone does.
            ram_mib: 2048,
            command: Vec::new(),
            env: Vec::new(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The image side of a run: pulled, stitched, or not in the cache at all.
    #[error("{0}")]
    Image(#[from] crate::image::Error),

    #[error("formatting {path}: {source}")]
    Format {
        path: String,
        #[source]
        source: arcbox_ext4::error::FormatError,
    },

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

    /// The machine started and its event loop then broke -- the one failure that can
    /// arrive after a guest is running, and the only way [`enter`] comes back at all.
    ///
    /// No source, because there is none to carry: libkrun logs what went wrong and
    /// returns nothing. `KRUN_LOG=debug` is where the answer is.
    #[error("the machine's event loop stopped; libkrun logged why (try KRUN_LOG=debug)")]
    EventLoop,

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

    pub(crate) fn vmm(context: impl Into<String>, source: krun::VmmError) -> Error {
        Error::Vmm {
            context: context.into(),
            source,
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;
