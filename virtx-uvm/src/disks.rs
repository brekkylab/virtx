//! How a snapshot's disk is laid out past its file, and how its machine opens it: here, a file
//! and nothing else, which libkrun reads and writes itself. An embedder that keeps more -- what
//! each part of the disk was restored from, fetched only as it is first read (virtx cloud's
//! lazy disks) -- says so with [`set_disks`], once, before anything here runs: in the server's
//! process and in each machine's, which is the same binary
//! ([`become_machine_if_spawned`](crate::enter_machine)).

use std::io;
use std::path::Path;
use std::sync::{Arc, OnceLock};

/// The storage a machine's block device reads and writes through, in place of its file.
pub use krun::DynStorage;

/// What a snapshot's disk is, past its file.
pub trait Disks: Send + Sync {
    /// The disk at `disk` was written again whole, or removed: whatever beside it says what it
    /// is goes with it.
    fn forget(&self, disk: &Path) -> io::Result<()> {
        let _ = disk;
        Ok(())
    }

    /// The disk at `from` was just moved to `to`: what is beside it moved with it, and what
    /// was beside the disk it replaced gone.
    fn moved(&self, from: &Path, to: &Path) -> io::Result<()> {
        let _ = (from, to);
        Ok(())
    }

    /// The disk at `from` was just copied to `to`, its machine paused: what is beside it copied
    /// as of now.
    fn copied(&self, from: &Path, to: &Path) -> io::Result<()> {
        let _ = (from, to);
        Ok(())
    }

    /// The disk at `disk`, a snapshot's, opened in its machine's process as its block device is
    /// to read it: `None` for its file as it is. `source` is what the server said the machine
    /// fetches from, as [`Hooks::chunks`](crate::sandbox::Hooks::chunks) answered.
    fn open(&self, disk: &Path, source: Option<&Path>) -> io::Result<Option<Arc<dyn MachineDisk>>> {
        let _ = (disk, source);
        Ok(None)
    }
}

/// A snapshot's disk as its machine has it open ([`Disks::open`]).
pub trait MachineDisk: Send + Sync {
    /// What its block device reads and writes through.
    fn storage(&self) -> Box<dyn DynStorage>;

    /// What it still has to do apart from its machine's reads, done on a thread of its own:
    /// returns once there is nothing.
    fn background(&self) {}

    /// Its machine booted: its agent is ready.
    fn booted(&self) {}

    /// Marked `mark`, as it is about to be copied while its machine runs
    /// ([`Machine::mark`](crate::Machine::mark)): whether it can be.
    fn mark(&self, mark: u64) -> io::Result<bool> {
        let _ = mark;
        Ok(false)
    }

    /// What the copy made at mark `mark` was kept as made what it is
    /// ([`Machine::rebase`](crate::Machine::rebase)): whether it was.
    fn rebase(&self, mark: u64) -> io::Result<bool> {
        let _ = mark;
        Ok(false)
    }
}

/// Disks that are files and nothing else.
struct Files;

impl Disks for Files {}

static DISKS: OnceLock<Arc<dyn Disks>> = OnceLock::new();

/// Snapshots' disks are `disks` in this process, from now on: said once, before any is made or
/// opened. A second is ignored.
pub fn set_disks(disks: Arc<dyn Disks>) {
    let _ = DISKS.set(disks);
}

/// What snapshots' disks are in this process.
pub(crate) fn disks() -> &'static dyn Disks {
    DISKS.get_or_init(|| Arc::new(Files)).as_ref()
}
