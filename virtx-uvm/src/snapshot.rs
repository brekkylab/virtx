//! Snapshots: the writable layers machines run over, each one ext4 image on the host holding
//! everything a machine wrote and nothing the image shipped.
//!
//! A disk and not a share, because overlayfs will take nothing else as an upper layer.
//! Formatted here rather than in the guest, so it works over an image nobody prepared.
//!
//! # Layout
//!
//! ```text
//! <root>/
//!   snapshot/<name>.ext4        one per snapshot, under the name it was made with
//!   snapshot/<name>.ext4.lock   what a machine over it, or a removal of it, holds
//! ```
//!
//! A writable ext4 takes one machine at a time, and nothing stops a second from opening it;
//! the lock is the agreement that does. Whoever boots a snapshot or removes it locks it first.
//! The lock is on a file beside the disk, not on the disk: on Windows a lock is mandatory, and
//! one on the disk would refuse the machine's own reads and writes.

use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::fstree::{self, FsTree};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0:?} is not a name: letters, digits, '.', '_' and '-', not starting with '.' or '-'")]
    InvalidName(String),

    #[error("no snapshot here is named {0}")]
    Unknown(String),

    #[error("a snapshot named {0} is already here")]
    AlreadyExists(String),

    #[error("snapshot {0} is in use by a machine")]
    InUse(String),

    #[error(transparent)]
    Fstree(#[from] fstree::Error),

    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: io::Error,
    },
}

impl Error {
    fn io(context: impl Into<String>, source: io::Error) -> Error {
        Error::Io {
            context: context.into(),
            source,
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub name: String,
    pub path: PathBuf,
}

/// A snapshot held for one machine or one removal, against every other in this process or
/// any other, until this drops -- or the process holding it ends.
#[derive(Debug)]
pub struct Lock(#[allow(dead_code)] File);

impl Snapshot {
    /// How large a snapshot may grow when nothing says otherwise.
    pub const SIZE: u64 = 2 << 30;

    /// How large it may grow, in bytes: what its filesystem was made to hold.
    pub fn capacity(&self) -> Result<u64> {
        Ok(fs::metadata(&self.path)
            .map_err(|e| Error::io(format!("reading {}", self.path.display()), e))?
            .len())
    }

    /// Made able to grow to `size` bytes, larger than it is, with what is on it kept. Its
    /// filesystem -- laid out by [`FsTree::to_ext4`] with no room to grow in place, which a
    /// kernel will not grow either -- is read whole and written again at `size`: a new image
    /// beside it, moved over it once whole, so a failure leaves it as it was. What it costs is
    /// what is written to it, not `size`. `_held` is its [lock](Snapshot::lock), which the
    /// caller holds throughout.
    pub fn grow(&self, size: u64, _held: &Lock) -> Result<()> {
        if self.capacity()? >= size {
            return Ok(());
        }
        let tree = FsTree::from_ext4(io::BufReader::new(
            File::open(&self.path)
                .map_err(|e| Error::io(format!("opening {}", self.path.display()), e))?,
        ))?;
        let part = self.path.with_extension("ext4.growing");
        let _ = fs::remove_file(&part);
        let written = File::create_new(&part)
            .map_err(|e| Error::io(format!("creating {}", part.display()), e))
            .and_then(|file| {
                tree.to_ext4(Snapshot::sparse(file), size)
                    .map_err(Error::from)
            })
            .and_then(|()| {
                fs::rename(&part, &self.path)
                    .map_err(|e| Error::io(format!("moving {} into place", part.display()), e))
            });
        // Written again whole: nothing beside it says what it is now.
        if written.is_ok() {
            crate::disks::disks().forget(&self.path).map_err(|e| {
                Error::io(format!("forgetting what {} was", self.path.display()), e)
            })?;
        }
        if written.is_err() {
            let _ = fs::remove_file(&part);
        }
        written
    }

    /// `file`, new and about to be a disk, kept sparse by the host: its size a ceiling and not
    /// an allocation.
    ///
    /// Unix keeps every file so. NTFS keeps one so only once it is marked: unmarked, a write
    /// past the end fills the gap with zeros first, which is seconds for each GiB, so formatting
    /// a 2 GiB disk -- whose last group's bitmaps are at its end -- writes it whole. A volume
    /// that cannot mark it (FAT) still takes the disk, at that cost.
    pub(crate) fn sparse(file: File) -> File {
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::System::{IO::DeviceIoControl, Ioctl::FSCTL_SET_SPARSE};
            let mut returned = 0;
            // SAFETY: the handle is `file`'s, open for the call, and nothing goes in or out.
            unsafe {
                DeviceIoControl(
                    file.as_raw_handle(),
                    FSCTL_SET_SPARSE,
                    std::ptr::null(),
                    0,
                    std::ptr::null_mut(),
                    0,
                    &mut returned,
                    std::ptr::null_mut(),
                );
            }
        }
        file
    }

    /// Hold it, refused while anything else does.
    pub fn lock(&self) -> Result<Lock> {
        let path = self.lock_path();
        let file =
            File::create(&path).map_err(|e| Error::io(format!("opening {}", path.display()), e))?;
        match file.try_lock() {
            Ok(()) => Ok(Lock(file)),
            Err(fs::TryLockError::WouldBlock) => Err(Error::InUse(self.name.clone())),
            Err(fs::TryLockError::Error(e)) => {
                Err(Error::io(format!("locking {}", path.display()), e))
            }
        }
    }

    /// Hold it, as [`lock`](Snapshot::lock) does, tried again while anything else holds it
    /// until `within` has passed: what holds it a moment -- a copy of it, or a process this one
    /// spawns, which shares every descriptor open as it forks until it execs -- is waited out.
    pub fn lock_within(&self, within: Duration) -> Result<Lock> {
        let began = Instant::now();
        loop {
            match self.lock() {
                Err(Error::InUse(_)) if began.elapsed() < within => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                held => return held,
            }
        }
    }

    /// Whether anything holds it now.
    pub fn in_use(&self) -> Result<bool> {
        match self.lock() {
            Ok(_) => Ok(false),
            Err(Error::InUse(_)) => Ok(true),
            Err(e) => Err(e),
        }
    }

    /// The bytes it takes on this host: what was written to it, not the ceiling it was made
    /// with, since the file is sparse.
    pub fn size(&self) -> Result<u64> {
        let metadata = fs::metadata(&self.path)
            .map_err(|e| Error::io(format!("reading {}", self.path.display()), e))?;
        #[cfg(unix)]
        let size = std::os::unix::fs::MetadataExt::blocks(&metadata) * 512;
        #[cfg(not(unix))]
        let size = metadata.len();
        Ok(size)
    }

    /// Delete its disk and its lock, whoever holds them: for a snapshot nothing else names.
    pub(crate) fn delete(&self) {
        let _ = fs::remove_file(&self.path);
        let _ = fs::remove_file(self.lock_path());
    }

    fn lock_path(&self) -> PathBuf {
        let mut path = self.path.as_os_str().to_owned();
        path.push(".lock");
        PathBuf::from(path)
    }
}

#[derive(Clone, Debug)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn new(root: impl Into<PathBuf>) -> Result<Store> {
        let root = root.into();
        let dir = root.join("snapshot");
        fs::create_dir_all(&dir).map_err(|e| Error::io(format!("making {}", dir.display()), e))?;
        Ok(Store { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Make an empty snapshot named `name`, that may hold up to `size` bytes. The size is a
    /// ceiling and not an allocation: the file is sparse.
    ///
    /// Formatted beside its name and linked under it once whole, so a snapshot that is listed
    /// is one that finished, and two creates of one name cannot both win.
    pub fn create(&self, name: &str, size: u64) -> Result<Snapshot> {
        let path = self.path(name)?;
        if path.exists() {
            return Err(Error::AlreadyExists(name.to_owned()));
        }
        let part = path.with_extension("ext4.part");
        let file = match File::create_new(&part) {
            Ok(file) => Snapshot::sparse(file),
            // Another create of this name, under way.
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                return Err(Error::AlreadyExists(name.to_owned()));
            }
            Err(e) => return Err(Error::io(format!("creating {}", part.display()), e)),
        };
        let formatted = FsTree::new()
            .to_ext4(file, size)
            .map_err(Error::from)
            .and_then(|()| match fs::hard_link(&part, &path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    Err(Error::AlreadyExists(name.to_owned()))
                }
                Err(e) => Err(Error::io(
                    format!("moving {} into place", path.display()),
                    e,
                )),
            });
        let _ = fs::remove_file(&part);
        formatted?;
        Ok(Snapshot {
            name: name.to_owned(),
            path,
        })
    }

    /// Snapshot `name`, which has to be here.
    pub fn open(&self, name: &str) -> Result<Snapshot> {
        let path = self.path(name)?;
        match path.is_file() {
            true => Ok(Snapshot {
                name: name.to_owned(),
                path,
            }),
            false => Err(Error::Unknown(name.to_owned())),
        }
    }

    /// Every snapshot here, in no set order.
    pub fn list(&self) -> Result<Vec<Snapshot>> {
        let dir = self.root.join("snapshot");
        let mut snapshots = Vec::new();
        for entry in
            fs::read_dir(&dir).map_err(|e| Error::io(format!("reading {}", dir.display()), e))?
        {
            let path = entry
                .map_err(|e| Error::io(format!("reading {}", dir.display()), e))?
                .path();
            if path.extension().is_none_or(|e| e != "ext4") {
                continue;
            }
            let Some(name) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            if crate::is_name(name) {
                snapshots.push(Snapshot {
                    name: name.to_owned(),
                    path: path.clone(),
                });
            }
        }
        Ok(snapshots)
    }

    /// Delete snapshot `name`, and everything a machine wrote to it -- refused while a machine
    /// runs over it.
    pub fn remove(&self, name: &str) -> Result<()> {
        let snapshot = self.open(name)?;
        // Held until the disk is gone, so nothing boots it in between.
        let lock = snapshot.lock()?;
        fs::remove_file(&snapshot.path)
            .map_err(|e| Error::io(format!("removing {}", snapshot.path.display()), e))?;
        let _ = crate::disks::disks().forget(&snapshot.path);
        drop(lock);
        let _ = fs::remove_file(snapshot.lock_path());
        Ok(())
    }

    /// Put the disk at `disk` in place as snapshot `name`, over the one there if any -- refused
    /// while a machine runs over that one. `disk` is moved, not copied, so it has to be on this
    /// store's filesystem: under [`root`](Store::root).
    pub fn replace(&self, name: &str, disk: &Path) -> Result<Snapshot> {
        let snapshot = Snapshot {
            name: name.to_owned(),
            path: self.path(name)?,
        };
        let lock = snapshot.lock()?;
        fs::rename(disk, &snapshot.path)
            .map_err(|e| Error::io(format!("moving {} into place", snapshot.path.display()), e))?;
        // What is beside it with it; and none left of the disk it replaced.
        crate::disks::disks()
            .moved(disk, &snapshot.path)
            .map_err(|e| {
                Error::io(
                    format!("moving what is beside {}", snapshot.path.display()),
                    e,
                )
            })?;
        drop(lock);
        Ok(snapshot)
    }

    /// Snapshot `name`'s disk, refused unless `name` [is one](crate::is_name).
    fn path(&self, name: &str) -> Result<PathBuf> {
        match crate::is_name(name) {
            true => Ok(self.root.join("snapshot").join(format!("{name}.ext4"))),
            false => Err(Error::InvalidName(name.to_owned())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch(Store);

    impl Scratch {
        fn new(name: &str) -> Scratch {
            let path = std::env::temp_dir()
                .join(format!("virtx-uvm-snapshot-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            Scratch(Store::new(path).unwrap())
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(self.0.root());
        }
    }

    #[test]
    fn a_new_snapshot_is_an_empty_filesystem() {
        let scratch = Scratch::new("empty");
        let snapshot = scratch.0.create("work", Snapshot::SIZE).unwrap();
        assert_eq!(
            snapshot.path,
            scratch.0.root().join("snapshot").join("work.ext4")
        );

        let read = FsTree::from_ext4(File::open(&snapshot.path).unwrap()).unwrap();
        let paths: Vec<_> = read.walk().into_iter().map(|(path, ..)| path).collect();
        assert_eq!(paths, [b"".to_vec(), b"lost+found".to_vec()]);
    }

    #[test]
    fn a_snapshot_is_there_until_it_is_removed() {
        let scratch = Scratch::new("remove");
        let store = &scratch.0;
        let one = store.create("one", Snapshot::SIZE).unwrap();
        let two = store.create("two", Snapshot::SIZE).unwrap();
        assert_eq!(store.open("one").unwrap(), one);
        assert!(matches!(
            store.create("one", Snapshot::SIZE),
            Err(Error::AlreadyExists(_))
        ));

        let mut listed = store.list().unwrap();
        listed.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(listed, [one, two.clone()]);

        store.remove("one").unwrap();
        assert!(matches!(store.remove("one"), Err(Error::Unknown(_))));
        assert!(matches!(store.open("one"), Err(Error::Unknown(_))));
        assert_eq!(store.list().unwrap(), [two]);
    }

    #[test]
    fn a_name_that_is_not_one_is_refused() {
        let scratch = Scratch::new("names");
        for name in ["", ".hidden", "-flag", "a/b", "a b"] {
            assert!(
                matches!(
                    scratch.0.create(name, Snapshot::SIZE),
                    Err(Error::InvalidName(_))
                ),
                "{name:?}"
            );
        }
    }

    #[test]
    fn a_snapshot_in_use_is_not_removed() {
        let scratch = Scratch::new("lock");
        let store = &scratch.0;
        let snapshot = store.create("held", Snapshot::SIZE).unwrap();
        assert!(!snapshot.in_use().unwrap());

        let held = snapshot.lock().unwrap();
        assert!(matches!(snapshot.lock(), Err(Error::InUse(name)) if name == "held"));
        assert!(snapshot.in_use().unwrap());
        assert!(matches!(store.remove("held"), Err(Error::InUse(_))));
        assert_eq!(
            store.list().unwrap(),
            std::slice::from_ref(&snapshot),
            "the lock was listed"
        );

        drop(held);
        store.remove("held").unwrap();
        assert!(
            fs::read_dir(store.root().join("snapshot"))
                .unwrap()
                .next()
                .is_none()
        );
    }

    /// Taken and let go of as processes are spawned: each, forked, shares the lock's descriptor
    /// until it execs, and so holds the lock a moment past its letting go.
    #[cfg(unix)]
    #[test]
    fn a_lock_a_spawned_process_shares_a_moment_is_waited_out() {
        use std::os::unix::process::CommandExt as _;
        use std::sync::atomic::{AtomicBool, Ordering};

        let scratch = Scratch::new("spawned");
        let snapshot = scratch.0.create("work", Snapshot::SIZE).unwrap();
        let spawning = AtomicBool::new(true);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                for _ in 0..500 {
                    let mut command = std::process::Command::new("true");
                    // SAFETY: nothing runs between the fork and the exec; a hook at all is
                    // what makes it a fork.
                    unsafe { command.pre_exec(|| Ok(())) };
                    command.status().unwrap();
                }
                spawning.store(false, Ordering::Relaxed);
            });
            while spawning.load(Ordering::Relaxed) {
                drop(snapshot.lock_within(Duration::from_secs(10)).unwrap());
            }
        });
    }

    #[test]
    fn a_disk_replaces_a_snapshot_unless_it_is_in_use() {
        let scratch = Scratch::new("replace");
        let store = &scratch.0;
        let old = store.create("work", Snapshot::SIZE).unwrap();
        let disk = store.root().join("incoming.ext4");
        fs::write(&disk, b"new").unwrap();

        let held = old.lock().unwrap();
        assert!(matches!(store.replace("work", &disk), Err(Error::InUse(_))));
        drop(held);
        assert_eq!(store.replace("work", &disk).unwrap(), old);
        assert_eq!(fs::read(&old.path).unwrap(), b"new");
        assert!(!disk.exists());

        // And one not here yet is put in place as it is.
        fs::write(&disk, b"other").unwrap();
        store.replace("other", &disk).unwrap();
        assert_eq!(
            fs::read(store.open("other").unwrap().path).unwrap(),
            b"other"
        );
    }

    #[test]
    fn a_part_formatted_snapshot_is_not_listed() {
        let scratch = Scratch::new("part");
        let store = &scratch.0;
        let part = store.path("half").unwrap().with_extension("ext4.part");
        fs::write(part, b"half").unwrap();
        assert!(store.list().unwrap().is_empty());
    }
}
