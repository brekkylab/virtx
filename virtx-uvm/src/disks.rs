//! What a sandbox's upper disk is past its file, and how its machine reads and writes it: here,
//! a file and nothing else, which libkrun reads and writes itself. An embedder that keeps more --
//! a disk whose data is fetched from elsewhere only as the guest first reads it, say -- says so
//! with [`set_disks`], once, before anything here runs: in the server's process and in each
//! machine's, which is the same binary ([`enter_machine`](crate::enter_machine)).
//!
//! A build's own disk, made and thrown away with its step, is a file whatever is set.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

/// The crate [`DynStorage`] is of, at the version libkrun links: what an embedder writes a
/// [`MachineDisk`]'s storage with.
pub use imago;
/// The storage a machine's block device reads and writes through, in place of its file.
pub use krun::DynStorage;

/// Anything the server's process holds for a machine's disk until the machine is down.
pub type Held = Box<dyn Send + Sync>;

/// What a sandbox's upper disk is, past its file.
pub trait Disks: Send + Sync {
    /// In the server's process, as a sandbox's machine is about to boot over the disk at `disk`,
    /// grown first where it was to be ([`whole`](Disks::whole)): what the machine fetches the
    /// disk's data from while it runs -- a path it is given, such as a socket -- and what
    /// answers there, held until the machine is down. `None` for a disk all in its file.
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

    /// In the server's process, its machine down: the disk at `disk` all in its file, about to
    /// be read whole and written again there -- grown, as a sandbox's disk is at the boot after
    /// it was resized larger. A failure leaves the disk as it was, not grown.
    fn whole(&self, disk: &Path) -> io::Result<()> {
        let _ = disk;
        Ok(())
    }

    /// In the server's process: the disk at `disk` removed with its sandbox, or as one left
    /// over, or written again whole ([`whole`](Disks::whole)): whatever beside it says what it
    /// was goes with it. Called once the disk's file is gone or new, where there is no one to
    /// answer to: a failure is logged and nothing more, and what the embedder kept for the disk
    /// is its own to remove.
    fn forget(&self, disk: &Path) -> io::Result<()> {
        let _ = disk;
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
}

/// Disks that are files and nothing else.
struct Files;

impl Disks for Files {}

static DISKS: OnceLock<Arc<dyn Disks>> = OnceLock::new();
/// Whether they are an embedder's, [set](set_disks) before anything here asked what they are.
static EMBEDDED: AtomicBool = AtomicBool::new(false);

/// Sandboxes' upper disks are `disks` in this process, from now on: said once, before any is
/// made or opened. `false`, and nothing changed, where it was said already or comes too late:
/// a disk was made or opened as a file first.
pub fn set_disks(disks: Arc<dyn Disks>) -> bool {
    let set = DISKS.set(disks).is_ok();
    if set {
        EMBEDDED.store(true, Ordering::Relaxed);
    }
    set
}

/// Whether sandboxes' upper disks in this process are an embedder's rather than files alone.
pub(crate) fn embedded() -> bool {
    EMBEDDED.load(Ordering::Relaxed)
}

/// What sandboxes' upper disks are in this process.
pub(crate) fn disks() -> &'static dyn Disks {
    DISKS.get_or_init(|| Arc::new(Files)).as_ref()
}
