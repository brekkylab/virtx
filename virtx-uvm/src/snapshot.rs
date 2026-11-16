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
//!   snapshot/<name>.ext4.lock   what a machine over it holds
//! ```
//!
//! A writable ext4 takes one machine at a time, and nothing stops a second from opening it;
//! the lock is the agreement that does. Whoever boots a snapshot locks it first.
//! The lock is on a file beside the disk, not on the disk: on Windows a lock is mandatory, and
//! one on the disk would refuse the machine's own reads and writes.

use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::fstree::{self, FsTree};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0:?} is not a name: letters, digits, '.', '_' and '-', not starting with '.' or '-'")]
    InvalidName(String),

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

/// A snapshot held for one machine, against every other in this process or
/// any other, until this drops -- or the process holding it ends.
#[derive(Debug)]
pub struct Lock(#[allow(dead_code)] File);

impl Snapshot {
    /// How large a snapshot may grow when nothing says otherwise.
    pub const SIZE: u64 = 2 << 30;

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
    /// Formatted beside its name and linked under it once whole, so a snapshot under its name
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
    fn a_name_is_made_once() {
        let scratch = Scratch::new("once");
        scratch.0.create("one", Snapshot::SIZE).unwrap();
        assert!(matches!(
            scratch.0.create("one", Snapshot::SIZE),
            Err(Error::AlreadyExists(_))
        ));
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
    fn a_snapshot_is_held_by_one_at_a_time() {
        let scratch = Scratch::new("lock");
        let snapshot = scratch.0.create("held", Snapshot::SIZE).unwrap();
        let held = snapshot.lock().unwrap();
        assert!(matches!(snapshot.lock(), Err(Error::InUse(name)) if name == "held"));
        drop(held);
        snapshot.lock().unwrap();
    }
}
