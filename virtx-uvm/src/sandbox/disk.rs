//! A sandbox's disk while its machine runs: its guest's writes put on it, and the disk copied
//! whole, where it is a file alone.

use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::oneshot;

use super::sandbox::Run;
use super::{Error, Sandbox};

/// How long a guest has to say it has synced ([`Sandbox::sync`]).
const SYNC_WAIT: Duration = Duration::from_secs(5);

impl Sandbox {
    /// Have its guest put what it has written on its disk, so that the disk's file holds it:
    /// `true` once the guest says it has, `false` if it has not within 5 s. A parked machine's
    /// disk holds what it wrote already. Refused once its machine has exited or it is deleted.
    pub async fn sync(&self) -> Result<bool, Error> {
        // Its machine neither boots nor stops meanwhile.
        let _turn = self.turn.lock().await;
        match self.run() {
            Some(run) => synced_by(&run).await,
            None if self.is_parked() => Ok(true),
            None => Err(Error::Ended),
        }
    }

    /// A copy of its disk at `to`, whole, replacing what is there: [synced](Self::sync) first
    /// if its machine runs, and the machine paused while the disk is copied -- at once where the
    /// host's filesystem clones files (APFS, Btrfs, XFS made with reflink), else for as long as
    /// reading and writing it takes. A guest that has not synced within 5 s is copied as it is,
    /// which its journal makes whole. A parked machine's disk is copied as it was left.
    ///
    /// For a disk that is its file alone: one the embedder's disks serve its machine
    /// ([`Disks::serve`](crate::disks::Disks::serve)) is theirs to copy, and refused here, as is
    /// one of an embedder's not yet booted in this process. Refused once its machine has exited
    /// or it is deleted.
    pub async fn copy_disk(&self, to: &Path) -> Result<(), Error> {
        // Its machine neither boots nor stops meanwhile, nor its disk goes.
        let _turn = self.turn.lock().await;
        let run = self.run();
        if run.is_none() && !self.is_parked() {
            return Err(Error::Ended);
        }
        let served = *self.served.lock().unwrap();
        if served.unwrap_or_else(crate::disks::embedded) {
            return Err(Error::Unsupported(format!(
                "sandbox {}'s disk is the embedder's to copy, not a file alone",
                self.record.id
            )));
        }
        let path = self.shared.snapshots.open(&self.record.id)?.path;
        if let Some(run) = &run {
            synced_by(run).await?;
        }
        let to = to.to_owned();
        tokio::task::spawn_blocking(move || {
            let _paused = run.as_ref().map(|run| run.machine.pause()).transpose()?;
            copy_sparse(&path, &to)
                .map_err(|e| Error::Failed(format!("copying {}: {e}", path.display())))
        })
        .await?
    }
}

/// `run`'s guest synced, as [`Sandbox::sync`] says, its sandbox's turn held.
async fn synced_by(run: &Arc<Run>) -> Result<bool, Error> {
    let (told, synced) = oneshot::channel();
    *run.synced.lock().unwrap() = Some(told);
    run.machine.sync()?;
    let synced = tokio::time::timeout(SYNC_WAIT, synced).await;
    Ok(matches!(synced, Ok(Ok(()))))
}

/// `from`, a disk, copied to `to`: cloned where the filesystem shares extents between files
/// ([`cloned`]) -- at once, however much it holds, which is what a sandbox paused for its copy
/// waits on -- and elsewhere with its holes left holes: only the extents with data in them read
/// and written, as `lseek` finds them.
#[cfg(unix)]
pub fn copy_sparse(from: &Path, to: &Path) -> io::Result<()> {
    use std::io::{Read as _, Seek as _, SeekFrom};
    use std::os::fd::AsRawFd as _;

    if cloned(from, to)? {
        return Ok(());
    }
    let mut from = std::fs::File::open(from)?;
    let mut to = std::fs::File::create(to)?;
    let length = from.metadata()?.len();
    to.set_len(length)?;
    let mut at = 0;
    while at < length {
        // SAFETY: a descriptor of ours, and a plain offset.
        let data = unsafe { libc::lseek(from.as_raw_fd(), at as libc::off_t, libc::SEEK_DATA) };
        if data == -1 {
            let e = io::Error::last_os_error();
            // No data past `at`: the rest is a hole.
            if e.raw_os_error() == Some(libc::ENXIO) {
                break;
            }
            return Err(e);
        }
        // SAFETY: as above.
        let hole = unsafe { libc::lseek(from.as_raw_fd(), data, libc::SEEK_HOLE) };
        if hole == -1 {
            return Err(io::Error::last_os_error());
        }
        let (data, hole) = (data as u64, hole as u64);
        from.seek(SeekFrom::Start(data))?;
        to.seek(SeekFrom::Start(data))?;
        io::copy(&mut (&mut from).take(hole - data), &mut to)?;
        at = hole;
    }
    to.sync_all()
}

/// `from`, a disk, copied to `to` whole.
#[cfg(not(unix))]
pub fn copy_sparse(from: &Path, to: &Path) -> io::Result<()> {
    std::fs::copy(from, to).map(|_| ())
}

/// `from` cloned to `to`, sharing its extents until either is written -- XFS made with reflink,
/// or Btrfs: true; false where the filesystem cannot, nothing copied, for the caller to copy it.
#[cfg(target_os = "linux")]
pub fn cloned(from: &Path, to: &Path) -> io::Result<bool> {
    use std::os::fd::AsRawFd as _;

    let source = std::fs::File::open(from)?;
    let target = std::fs::File::create(to)?;
    // SAFETY: two descriptors of ours, open for as long as the call.
    let done = unsafe { libc::ioctl(target.as_raw_fd(), libc::FICLONE, source.as_raw_fd()) };
    if done == 0 {
        target.sync_all()?;
        return Ok(true);
    }
    let e = io::Error::last_os_error();
    match e.raw_os_error() {
        // Not a filesystem that clones, or not between these two.
        Some(libc::EOPNOTSUPP | libc::EXDEV | libc::EINVAL | libc::ENOTTY) => Ok(false),
        _ => Err(e),
    }
}

/// `from` cloned to `to` on APFS, which clones any file: true; false where it cannot.
#[cfg(target_os = "macos")]
pub fn cloned(from: &Path, to: &Path) -> io::Result<bool> {
    use std::os::unix::ffi::OsStrExt as _;

    let c = |p: &Path| {
        std::ffi::CString::new(p.as_os_str().as_bytes())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))
    };
    let (source, target) = (c(from)?, c(to)?);
    // `clonefile` makes `to`; one there already is replaced.
    match std::fs::remove_file(to) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    // SAFETY: two NUL-terminated paths of ours.
    if unsafe { libc::clonefile(source.as_ptr(), target.as_ptr(), 0) } == 0 {
        return Ok(true);
    }
    let e = io::Error::last_os_error();
    match e.raw_os_error() {
        Some(libc::ENOTSUP | libc::EXDEV) => Ok(false),
        _ => Err(e),
    }
}

/// Nowhere else does a copy clone.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn cloned(_from: &Path, _to: &Path) -> io::Result<bool> {
    Ok(false)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn a_disk_copied_keeps_its_data_and_its_holes() {
        use std::io::{Seek as _, SeekFrom, Write as _};
        use std::os::unix::fs::MetadataExt as _;

        let dir = std::env::temp_dir().join(format!("virtx-sparse-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (from, to) = (dir.join("from.img"), dir.join("to.img"));
        let mut disk = std::fs::File::create(&from).unwrap();
        disk.set_len(64 << 20).unwrap();
        for (at, what) in [(0, b"first"), (40 << 20, b"later")] {
            disk.seek(SeekFrom::Start(at)).unwrap();
            disk.write_all(what).unwrap();
        }
        disk.sync_all().unwrap();

        copy_sparse(&from, &to).unwrap();
        assert_eq!(std::fs::read(&from).unwrap(), std::fs::read(&to).unwrap());
        let blocks = std::fs::metadata(&to).unwrap().blocks() * 512;
        assert!(blocks < 4 << 20, "{blocks} bytes on disk for 10 of data");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
