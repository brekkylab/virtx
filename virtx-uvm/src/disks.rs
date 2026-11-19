//! What a sandbox's upper disk is past its file, and how its machine reads and writes it: here,
//! a file and nothing else, which libkrun reads and writes itself. An embedder that keeps more --
//! a disk whose data is fetched from elsewhere only as the guest first reads it, say -- says so
//! with [`set_disks`], once, before anything here runs: in the server's process and in each
//! machine's, which is the same binary ([`enter_machine`](crate::enter_machine)).
//!
//! A build's own disk, made and thrown away with its step, is a file whatever is set.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

/// The storage a machine's block device reads and writes through, in place of its file.
pub use krun::DynStorage;

/// Anything the server's process holds for a machine's disk until the machine is down.
pub type Held = Box<dyn Send + Sync>;

/// What a sandbox's upper disk is, past its file.
pub trait Disks: Send + Sync {
    /// In the server's process, as a sandbox's machine is about to boot over the disk at `disk`:
    /// what the machine fetches the disk's data from while it runs -- a path it is given, such
    /// as a socket -- and what answers there, held until the machine is down. `None` for a disk
    /// all in its file.
    fn serve(&self, disk: &Path) -> io::Result<Option<(PathBuf, Held)>> {
        let _ = disk;
        Ok(None)
    }

    /// In the machine's process: the disk at `disk` opened as its block device is to read and
    /// write it, `source` being what [`serve`](Disks::serve) answered. `None` for its file as
    /// it is.
    fn open(&self, disk: &Path, source: Option<&Path>) -> io::Result<Option<Arc<dyn MachineDisk>>> {
        let _ = (disk, source);
        Ok(None)
    }

    /// In the server's process: the disk at `disk` removed with its sandbox, or as one left
    /// over: whatever beside it says what it is goes with it.
    fn forget(&self, disk: &Path) -> io::Result<()> {
        let _ = disk;
        Ok(())
    }

    /// In the server's process: the disk at `disk` was just copied to `to`
    /// ([`Sandbox::copy_disk`](crate::Sandbox::copy_disk)), its machine paused, if it runs,
    /// until this returns: whatever beside it says what it is copied as of now.
    fn copied(&self, disk: &Path, to: &Path) -> io::Result<()> {
        let _ = (disk, to);
        Ok(())
    }
}

/// A sandbox's upper disk as its machine has it open ([`Disks::open`]).
pub trait MachineDisk: Send + Sync {
    /// What its block device reads and writes through.
    fn storage(&self) -> Box<dyn DynStorage>;

    /// What it still has to do apart from its machine's reads, done on a thread of its own:
    /// returns once there is nothing.
    fn background(&self) {}

    /// Its machine booted: the guest's agent is ready.
    fn booted(&self) {}

    /// The guest has put what it had written on it, and its machine is about to be paused for
    /// it to be copied ([`Sandbox::copy_disk`](crate::Sandbox::copy_disk)): a point to tell
    /// what is written from now on apart from what the copy holds. What the guest says next
    /// waits on it, so it is brief. Where the guest took longer to sync than `copy_disk`
    /// waits, it comes after the copy.
    fn synced(&self) {}
}

/// Disks that are files and nothing else.
struct Files;

impl Disks for Files {}

static DISKS: OnceLock<Arc<dyn Disks>> = OnceLock::new();

/// Sandboxes' upper disks are `disks` in this process, from now on: said once, before any is
/// made or opened. A second is ignored.
pub fn set_disks(disks: Arc<dyn Disks>) {
    let _ = DISKS.set(disks);
}

/// What sandboxes' upper disks are in this process.
pub(crate) fn disks() -> &'static dyn Disks {
    DISKS.get_or_init(|| Arc::new(Files)).as_ref()
}
