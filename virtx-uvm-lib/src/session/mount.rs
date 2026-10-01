//! A host directory the guest can see.
//!
//! # Why a share and not a disk
//!
//! The opposite of the [rootfs](super::Ext4)'s reason. What an upper layer holds is a
//! Linux filesystem the guest owns, so it has to be a block device. What a
//! mount holds is a host directory the host goes on using while the guest reads it, so it
//! has to be a share -- a block device would mean a filesystem image, which is not a
//! directory anybody on the host could open.
//!
//! virtio-fs is what carries it, and the guest mounts the share by the tag the host
//! attached it under. Both halves are [`Session::enter`](super::Session::enter)'s to
//! arrange; what is here is only what a caller says.
//!
//! # Read-only is enforced on the host
//!
//! [`read_only`](Mount::read_only) attaches the device read-only, which makes the
//! virtiofs server itself refuse every write. That is deliberately not the same as
//! mounting it `ro` in the guest: a mount flag is the guest's, and a guest with a root
//! can remount its own filesystems. The server is on the other side of the boundary, so
//! what it refuses stays refused.
//!
//! What a guest sees when it writes anyway is an error from the write, not from the
//! mount -- `/proc/mounts` in the guest says `rw` for a share the host will not let it
//! write to.

use std::path::PathBuf;

/// A host directory, and where the guest sees it.
///
/// ```no_run
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// use virtx_uvm::session::{Mount, Session};
///
/// Session::new()
///     .image("alpine:3.21")
///     .mount(Mount::new("./data", "/data").read_only())
///     .command(["ls", "/data"])
///     .enter()?;
/// # Ok(())
/// # }
/// ```
///
/// Nothing is created on either side: the host directory has to be there, and
/// [`at`](Mount::at) is made in the guest if it is not. A mount lands on the root the
/// workload sees, which is the overlay -- so a directory made for it is recorded on the
/// upper layer and found again by the next session over the same one.
#[derive(Clone, Debug)]
pub struct Mount {
    /// The directory on the host. It has to exist: there is nothing sensible to share
    /// from a path that is not there, and creating it would be this deciding what a
    /// caller meant by a typo.
    pub host: PathBuf,

    /// Where it appears in the guest, which has to be absolute. Made if it is not there.
    pub at: PathBuf,

    /// Whether the host refuses the guest's writes. See the module doc -- it is the
    /// device that refuses them, not the guest's own mount flags.
    pub read_only: bool,
}

impl Mount {
    /// Share `host` with the guest, where it appears at `at`. Writable, as a share is
    /// unless something says otherwise.
    pub fn new(host: impl Into<PathBuf>, at: impl Into<PathBuf>) -> Mount {
        Mount {
            host: host.into(),
            at: at.into(),
            read_only: false,
        }
    }

    /// Attach it read-only, so that the host refuses every write the guest makes to it.
    pub fn read_only(mut self) -> Mount {
        self.read_only = true;
        self
    }
}
