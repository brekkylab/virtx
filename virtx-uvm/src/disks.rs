//! What a sandbox's disk is past its file, and how its machine reads and writes it: here, a
//! file and nothing else, which libkrun reads and writes itself. An embedder that keeps more --
//! a disk whose data is fetched from elsewhere only as the guest first reads it (virtx cloud's
//! lazy disks), what each part of it was restored from -- says so with [`set_disks`], once,
//! before anything here runs: in the server's process and in each machine's, which is the same
//! binary ([`enter_machine`](crate::enter_machine)).
//!
//! A build's own disk, made and thrown away with its step, is a file whatever is set.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

/// The storage a machine's block device reads and writes through, in place of its file.
pub use krun::DynStorage;

/// Anything the server's process holds for a machine's disk until the machine is down.
pub type Held = Box<dyn Send + Sync>;

/// What a sandbox's disk is, past its file.
pub trait Disks: Send + Sync {
    /// In the server's process, as a sandbox's machine is about to boot over the disk at `disk`:
    /// what the machine fetches the disk's data from while it runs -- a path it is given, such
    /// as a socket -- and what answers there, held until the machine is down. `None` for a disk
    /// all in its file, or one a server's [`Hooks::chunks`](crate::sandbox::Hooks::chunks)
    /// serves.
    fn serve(&self, disk: &Path) -> io::Result<Option<(PathBuf, Held)>> {
        let _ = disk;
        Ok(None)
    }

    /// In the server's process: the disk at `disk` written again whole, or removed with its
    /// sandbox, or as one left over: whatever beside it says what it is goes with it.
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

    /// In the machine's process: the disk at `disk` opened as its block device is to read and
    /// write it, `source` being what [`serve`](Disks::serve), or the server's
    /// [`Hooks::chunks`](crate::sandbox::Hooks::chunks), answered. `None` for its file as it is.
    fn open(&self, disk: &Path, source: Option<&Path>) -> io::Result<Option<Arc<dyn MachineDisk>>> {
        let _ = (disk, source);
        Ok(None)
    }

    /// The disk at `disk`, a snapshot's that no machine runs on, opened in the server's process
    /// to be read there, held meanwhile: `None` for its file as it is. `source` is what the
    /// server fetches it from, as [`open`](Disks::open) takes it. Only read: what it fetches it
    /// keeps, as a machine's reads do, and nothing it reads is said to be its machine's boot.
    fn read(&self, disk: &Path, source: Option<&Path>) -> io::Result<Option<Box<dyn DiskRead>>> {
        let _ = (disk, source);
        Ok(None)
    }
}

/// A snapshot's disk opened to be read in the server's process ([`Disks::read`]).
pub trait DiskRead: Send {
    /// `buf.len()` bytes of it from `offset`, all of them within it.
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()>;
}

/// A disk's file, read as it is.
struct Plain(File);

impl DiskRead for Plain {
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        #[cfg(unix)]
        return std::os::unix::fs::FileExt::read_exact_at(&self.0, buf, offset);
        #[cfg(windows)]
        {
            let (mut buf, mut offset) = (buf, offset);
            while !buf.is_empty() {
                match std::os::windows::fs::FileExt::seek_read(&self.0, buf, offset)? {
                    0 => return Err(io::ErrorKind::UnexpectedEof.into()),
                    n => {
                        buf = &mut std::mem::take(&mut buf)[n..];
                        offset += n as u64;
                    }
                }
            }
            Ok(())
        }
    }
}

/// A snapshot's disk as the server's process reads it, through what its disks read it with
/// ([`Disks::read`]), read and sought as a file is.
pub(crate) struct Image {
    disk: Box<dyn DiskRead>,
    at: u64,
    len: u64,
}

impl Image {
    /// The disk at `disk`, fetched from `source` where it is fetched as it is read.
    pub(crate) fn open(disk: &Path, source: Option<&Path>) -> io::Result<Image> {
        let len = std::fs::metadata(disk)?.len();
        let disk = match disks().read(disk, source)? {
            Some(read) => read,
            None => Box::new(Plain(File::open(disk)?)),
        };
        Ok(Image { disk, at: 0, len })
    }
}

impl Read for Image {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = buf.len().min(self.len.saturating_sub(self.at) as usize);
        self.disk.read_exact_at(&mut buf[..n], self.at)?;
        self.at += n as u64;
        Ok(n)
    }
}

impl Seek for Image {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let at = match pos {
            SeekFrom::Start(at) => Some(at),
            SeekFrom::End(by) => self.len.checked_add_signed(by),
            SeekFrom::Current(by) => self.at.checked_add_signed(by),
        };
        self.at = at.ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
        Ok(self.at)
    }
}

/// A sandbox's disk as its machine has it open ([`Disks::open`]).
pub trait MachineDisk: Send + Sync {
    /// What its block device reads and writes through.
    fn storage(&self) -> Box<dyn DynStorage>;

    /// What it still has to do apart from its machine's reads, done on a thread of its own:
    /// returns once there is nothing.
    fn background(&self) {}

    /// Its machine booted: the guest's agent is ready.
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

/// Sandboxes' disks are `disks` in this process, from now on: said once, before any is made or
/// opened. A second is ignored.
pub fn set_disks(disks: Arc<dyn Disks>) {
    let _ = DISKS.set(disks);
}

/// What sandboxes' disks are in this process.
pub(crate) fn disks() -> &'static dyn Disks {
    DISKS.get_or_init(|| Arc::new(Files)).as_ref()
}
