//! A sandbox's disk read whole while its machine runs -- copied as the machine has it, and
//! marked as it is, for the copy to become the disk's base once it is kept -- and committed
//! to an image, running or stopped.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
#[cfg(unix)]
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(unix)]
use std::time::Duration;

#[cfg(unix)]
use tokio::sync::oneshot;

use super::{Error, Sandbox};
use crate::{Digest, ImageStoreError, Reference, written_layer};

/// How long a guest has to say it has synced before its disk is copied as it is.
#[cfg(unix)]
const SYNC_WAIT: Duration = Duration::from_secs(5);
/// How long a machine is waited on to mark its disk, or rebase it.
#[cfg(unix)]
const MARK_WAIT: Duration = Duration::from_secs(5);
/// The next mark of a running machine's disk, as it is copied: one per copy, in this process.
#[cfg(unix)]
static NEXT_MARK: AtomicU64 = AtomicU64::new(1);

impl Sandbox {
    /// A copy of its disk at `to`, as its machine has it: what the guest has written put on it
    /// first, and the machine paused while it is copied -- for as long as that takes -- so that
    /// the copy is whole. The disk is marked as it is copied: once the copy is kept, what it
    /// was kept as can be made the disk's base ([`rebase`](Self::rebase)) -- the mark's, if the
    /// disk could be marked. `None`, and no copy, where its machine is not up.
    #[cfg(unix)]
    pub async fn copy_disk(&self, to: &Path) -> Result<Option<Option<u64>>, Error> {
        let Some(run) = self.run() else {
            return Ok(None);
        };
        let path = self.shared.snapshots.open(&self.record.id)?.path;
        let (told, synced) = oneshot::channel();
        *run.synced.lock().unwrap() = Some(told);
        run.machine.sync()?;
        // Copied as the machine has it either way: a guest that does not say it has synced in
        // time is paused as it is, which ext4's journal makes whole.
        let _ = tokio::time::timeout(SYNC_WAIT, synced).await;
        // Marked after the sync, before the pause: what is written from the mark on is in the
        // copy or noted apart, or both.
        let mark = NEXT_MARK.fetch_add(1, Ordering::Relaxed);
        let (told, marked) = oneshot::channel();
        *run.marked.lock().unwrap() = Some((mark, told));
        let mark = match run.machine.mark(mark) {
            Ok(()) => match tokio::time::timeout(MARK_WAIT, marked).await {
                Ok(Ok(Ok(true))) => Some(mark),
                Ok(Ok(Err(e))) => {
                    eprintln!("{}'s disk not marked: {e}", self.record.id);
                    None
                }
                _ => None,
            },
            Err(_) => None,
        };
        let to = to.to_owned();
        tokio::task::spawn_blocking(move || {
            let _paused = run.machine.pause()?;
            copy_sparse(&path, &to)
                .map_err(|e| Error::Failed(format!("copying {}: {e}", path.display())))?;
            // What is beside it copied as of this pause.
            crate::disks::disks().copied(&path, &to).map_err(|e| {
                Error::Failed(format!("copying what is beside {}: {e}", path.display()))
            })?;
            Ok::<_, Error>(())
        })
        .await??;
        Ok(Some(mark))
    }

    /// What the copy of its disk made at `mark` ([`copy_disk`](Self::copy_disk)) was kept as,
    /// made what its disk is while its machine runs, as the server's disks have it
    /// ([`MachineDisk::rebase`](crate::disks::MachineDisk::rebase)) -- what it was kept as
    /// beside the disk already, as they keep it. Whether it was: not if its machine is down
    /// now, or another mark came since.
    #[cfg(unix)]
    pub async fn rebase(&self, mark: u64) -> Result<bool, Error> {
        let Some(run) = self.run() else {
            return Ok(false);
        };
        let (told, rebased) = oneshot::channel();
        *run.rebased.lock().unwrap() = Some((mark, told));
        run.machine.rebase(mark)?;
        match tokio::time::timeout(MARK_WAIT, rebased).await {
            Ok(Ok(Ok(rebased))) => Ok(rebased),
            Ok(Ok(Err(e))) => Err(Error::Failed(format!(
                "rebasing {}'s disk: {e}",
                self.record.id
            ))),
            _ => Ok(false),
        }
    }

    /// An image of it, tagged `reference`: the image it stands on, with what it has written as
    /// one more layer, and that image's config. Of a running sandbox, its disk is copied as its
    /// machine has it, paused meanwhile ([`copy_disk`](Self::copy_disk)); of a stopped one, as
    /// it was left. Neither uses the sandbox, nor starts it.
    ///
    /// Where a machine cannot be paused (Windows), a running sandbox is refused: it is
    /// committed once it is stopped.
    pub async fn commit(
        self: &Arc<Self>,
        reference: &Reference,
        report: &mut (dyn FnMut(String) + Send),
    ) -> Result<Digest, Error> {
        let shared = &self.shared;
        let id = &self.record.id;
        report(format!("committing sandbox {id}"));
        let (image, mut layers) = shared
            .images
            .get_image(&self.record.image)?
            .ok_or(ImageStoreError::Missing(self.record.image.clone()))?;
        let scratch = Scratch::new()?;
        let copy = scratch.0.join("disk.ext4");
        {
            // Not started meanwhile, if it is stopped.
            let turn = self.turn.lock().await;
            if !self.is_running() {
                // Its disk as it was left, brought up to what is kept first.
                shared.hooks.reading(id).await?;
                let from = shared.snapshots.open(id)?.path;
                let to = copy.clone();
                tokio::task::spawn_blocking(move || copy_sparse(&from, &to))
                    .await?
                    .map_err(|e| Error::Failed(format!("copying its disk: {e}")))?;
                drop(turn);
            } else {
                drop(turn);
                #[cfg(unix)]
                {
                    if self.copy_disk(&copy).await?.is_none() {
                        return Err(Error::Stopped(format!(
                            "sandbox {id} stopped as it was committed"
                        )));
                    }
                    // Made all here, to be read, as the server's disks have it.
                    shared.hooks.materialize(id, &copy).await?;
                }
                #[cfg(not(unix))]
                return Err(Error::Running(format!(
                    "sandbox {id} runs: it is committed once it is stopped here"
                )));
            }
        }
        report("filing what it wrote as a layer".into());
        let images = shared.images.clone();
        let layer = tokio::task::spawn_blocking(move || written_layer(&images, &copy)).await??;
        let layered = layer.is_some();
        layers.extend(layer);
        let mut image = image;
        crate::recipe_forgotten(&mut image);
        crate::history_kept(
            &mut image,
            format!("virtx commit {id}"),
            layered,
            layers.len(),
        );
        let digest = shared.images.insert_image(&image, &layers)?;
        shared.images.insert_tag(reference, &digest)?;
        Ok(digest)
    }
}

/// A directory of its own under the host's temporary one, removed as this drops.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Result<Scratch, Error> {
        let dir = std::env::temp_dir().join(format!(
            "virtx-commit-{}-{}",
            std::process::id(),
            super::sandbox::pick(|_| false)
        ));
        std::fs::create_dir_all(&dir)
            .map_err(|e| Error::Failed(format!("making {}: {e}", dir.display())))?;
        Ok(Scratch(dir))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
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
