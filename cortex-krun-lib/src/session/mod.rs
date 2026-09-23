//! Running an image: the writable layer a session gets, and the machine that runs on it.
//!
//! # What a session is
//!
//! One boot of one image, with one file to show for it. [`image`](crate::image) answers
//! with a [`Disk`](crate::image::Disk) -- the layers stitched into a single read-only
//! EROFS filesystem -- and that disk is shared by every session ever started from the
//! image. What makes a session its own is its [`Ext4`] rootfs: an ext4 image the guest
//! stacks over that disk, holding every byte it writes and nothing anyone else wrote.
//!
//! ```text
//!   /dev/vdb  ext4,  writable    this session's upper layer ─┐
//!                                                            ├─ overlay ──► /
//!   /dev/vda  erofs, read-only   the image's disk           ─┘
//! ```
//!
//! So a session is thrown away by deleting one file, and carried on by keeping it: a
//! second [`Session::enter`] over the same upper layer starts where the first one
//! stopped, because the overlay's upperdir is a directory on a disk that outlived the
//! machine. A session that names no path is the same machine over a file unlinked the
//! moment the machine holds it, which is how a run that is meant to leave nothing leaves
//! nothing.
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
//! [`Session::enter`] enters the VMM, and the VMM exits the process when the guest goes
//! down. So it returns only by failing, which is what the
//! [`Infallible`](std::convert::Infallible) in its signature says: everything after it in
//! a caller is unreachable. That is also why a session has one operation and not two --
//! nothing can run after a session starts, so nothing can ask a second thing of it.

mod ext4;
#[cfg(feature = "gpu")]
mod gpu;
mod mount;
mod net;
mod session;

pub use ext4::Ext4;
pub use mount::Mount;
pub use net::Network;
pub use session::Session;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A session entered without an image, which is the one thing about it that has no
    /// default -- see [`Session::image`].
    #[error("a session has no image; name one with Session::image")]
    NoImage,

    /// The image side of a run: pulled, stitched, or not in the cache at all.
    #[error("{0}")]
    Image(#[from] crate::image::Error),

    /// A mount the guest could not be told where to put. A mount point is a path the
    /// guest resolves on its own root, and one that is not absolute would land wherever
    /// the init happened to be standing.
    #[error("{at} is not absolute, which a mount point in the guest has to be")]
    MountPoint { at: String },

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
