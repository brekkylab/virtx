//! A parked sandbox's disk read in the server's process, its machine left parked: what a read
//! of its files is answered from while it is ([`Files`](crate::Files)).
//!
//! What is on a parked sandbox's disk changes as its machine boots, or as its file is put in
//! place again, and it is in one [era](Era) from one to the next: what was read of it in an era
//! is what is on it for as long as that era lasts. Each read holds the sandbox's turn, which a
//! boot, a park and a deletion take, and its disk's lock, which anything else that writes the
//! disk takes, for as long as the read alone.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use super::sandbox::{ABIN_HOST, DISK_WAIT, abin_of, abin_programs};
use super::{Error, Sandbox};
use crate::disks::Image;
use crate::{SnapshotError, Volume};

/// The next boot of a disk in this process, which no disk here has had.
static ERAS: AtomicU64 = AtomicU64::new(1);

/// A boot no disk in this process has had.
pub(super) fn next_era() -> u64 {
    ERAS.fetch_add(1, Ordering::Relaxed)
}

/// What a parked sandbox's disk is in: the boot it was last written by, and its file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Era {
    boots: u64,
    /// The file's own number, or on Windows when it was made, and its length: `None` for no
    /// disk made yet.
    file: Option<(u64, u64)>,
}

/// What the disk at `disk` is now, as an [`Era`] tells it apart: `None` for none there.
fn file(disk: &Path) -> Option<(u64, u64)> {
    let meta = std::fs::metadata(disk).ok()?;
    #[cfg(unix)]
    let which = std::os::unix::fs::MetadataExt::ino(&meta);
    #[cfg(not(unix))]
    let which = meta
        .created()
        .ok()
        .and_then(|at| at.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |since| since.as_nanos() as u64);
    Some((which, meta.len()))
}

/// What a machine shows over its disk at a path: a volume, or a directory of this host's,
/// read-only.
pub(crate) enum Shown {
    Volume(Arc<Volume>),
    Dir(PathBuf),
}

impl Sandbox {
    /// Its disk's era while its machine is parked: `None` while it is up, once it has exited,
    /// and once it is deleted. Not while its machine boots or parks, which is waited out.
    pub(crate) async fn parked_era(&self) -> Option<Era> {
        let _turn = self.turn.lock().await;
        self.era_now()
    }

    /// Its disk's era now, if its machine is parked. Asked under its turn.
    fn era_now(&self) -> Option<Era> {
        if !self.is_parked() {
            return None;
        }
        let disk = self.shared.snapshots.open(&self.record.id).ok();
        Some(Era {
            boots: self.era.load(Ordering::Relaxed),
            file: disk.and_then(|disk| file(&disk.path)),
        })
    }

    /// `read` done on a blocking thread with its disk as it is in era `era`, while its machine
    /// is parked: `None`, nothing read, once it is not, or its disk is in another era. `read`
    /// is given no disk where none was made for it yet.
    ///
    /// Held while `read` runs: its turn, its disk's lock, and what the hooks hold for it
    /// ([`Hooks::reading`](super::Hooks::reading)). Its disk is read through the server's disks
    /// ([`Disks::read`](crate::disks::Disks::read)), from where the hooks fetch it
    /// ([`Hooks::chunks`](super::Hooks::chunks)).
    pub(crate) async fn read_parked<T: Send + 'static>(
        &self,
        era: Era,
        read: impl FnOnce(Option<&mut Image>) -> T + Send + 'static,
    ) -> Result<Option<T>, Error> {
        let _turn = self.turn.lock().await;
        if self.era_now() != Some(era) {
            return Ok(None);
        }
        let id = self.record.id.clone();
        let disk = match self.shared.snapshots.open(&id) {
            Ok(disk) => disk,
            Err(SnapshotError::Unknown(_)) => {
                return Ok(Some(tokio::task::spawn_blocking(move || read(None)).await?));
            }
            Err(e) => return Err(e.into()),
        };
        let hooked = self.shared.hooks.reading(&id).await?;
        let locking = disk.clone();
        let lock = tokio::task::spawn_blocking(move || locking.lock_within(DISK_WAIT)).await??;
        let (source, serving) = self.shared.hooks.chunks(&id, &disk.path)?.unzip();
        let read = tokio::task::spawn_blocking(move || {
            let mut image = Image::open(&disk.path, source.as_deref()).map_err(|e| {
                Error::Failed(format!("opening {} to read: {e}", disk.path.display()))
            })?;
            Ok::<_, Error>(read(Some(&mut image)))
        })
        .await??;
        drop((serving, lock, hooked));
        Ok(Some(read))
    }

    /// The layers of the image it stands on, bottom first, each its file here.
    pub(crate) fn layers(&self) -> Result<Vec<PathBuf>, Error> {
        let images = &self.shared.images;
        let (_, layers) = images
            .get_image(&self.record.image)?
            .ok_or_else(|| Error::NotFound(format!("no image here is {}", self.record.image)))?;
        Ok(layers
            .iter()
            .map(|layer| images.layer_path(layer))
            .collect())
    }

    /// What a machine of it booted now shows over its disk, each at its path in the guest, in
    /// the order it is mounted, a later one landing over an earlier one: `/abin` if it has
    /// programs there, `/alib` if this host has one, and its volumes. Blocking work: its
    /// `/abin` is made here the first time any sandbox asks for its programs.
    pub(crate) fn shown(&self) -> Result<Vec<(String, Shown)>, Error> {
        let shared = &self.shared;
        let mut shown = Vec::new();
        if !self.record.args.abins.is_empty() {
            let programs = abin_programs(&shared.vm);
            let under = shared.kept.join(ABIN_HOST);
            let dir = abin_of(&under, &programs, &self.record.args.abins)?;
            shown.push(("/abin".to_owned(), Shown::Dir(dir)));
        }
        if let Some(dir) = crate::machine::alib() {
            shown.push(("/alib".to_owned(), Shown::Dir(dir)));
        }
        for asked in &self.record.args.volumes {
            let Ok(at) = crate::mount_point(&asked.path) else {
                continue;
            };
            let volume = shared.volumes.open(&asked.volume)?;
            shown.push((at, Shown::Volume(Arc::new(volume))));
        }
        Ok(shown)
    }
}
