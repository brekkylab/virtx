//! The upper disks sandboxes write over their images to, one ext4 each, kept in one directory
//! under the sandboxes' ids.
//!
//! ```text
//! <dir>/<id>.ext4        the disk
//! <dir>/<id>.ext4.lock   what the sandbox holds for as long as it is there
//! ```
//!
//! A writable ext4 takes one machine at a time, and nothing stops a second from opening it;
//! the lock is the agreement that does. It is on a file beside the disk, not on the disk: on
//! Windows a lock is mandatory, and one on the disk would refuse the machine's own reads and
//! writes.

use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};

use super::Error;

/// An upper disk, held from before it was made until this drops, which deletes it -- or until
/// the process holding it ends, which leaves it to [`Upper::sweep`].
#[derive(Debug)]
pub(super) struct Upper {
    pub(super) id: String,
    pub(super) path: PathBuf,
    _lock: File,
}

impl Upper {
    /// Make an empty disk under `dir` for sandbox `id`, that may grow to `gib` GiB, or `None`
    /// for an id another holds or has made.
    pub(super) fn create(dir: &Path, id: &str, gib: u32) -> Result<Option<Upper>, Error> {
        fs::create_dir_all(dir).map_err(|e| io(format!("making {}", dir.display()), e))?;
        let path = dir.join(format!("{id}.ext4"));
        let Some(lock) = hold(&path)? else {
            return Ok(None);
        };
        if path.exists() {
            return Ok(None);
        }
        let upper = Upper {
            id: id.to_owned(),
            path,
            _lock: lock,
        };
        // Dropped on a failure, so the lock goes with a disk that was not made.
        crate::machine::create_upper(&upper.path, gib)?;
        Ok(Some(upper))
    }

    /// Delete every disk under `dir` that nothing holds: those of sandboxes whose process ended
    /// without closing them.
    pub(super) fn sweep(dir: &Path) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "ext4")
                && let Ok(Some(_held)) = hold(&path)
            {
                let _ = delete(&path);
            }
        }
    }
}

impl Drop for Upper {
    fn drop(&mut self) {
        let _ = delete(&self.path);
    }
}

/// Hold the lock of the disk at `path`, or `None` while anything else does.
fn hold(path: &Path) -> Result<Option<File>, Error> {
    let lock = lock_path(path);
    let file = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock)
        .map_err(|e| io(format!("opening {}", lock.display()), e))?;
    match file.try_lock() {
        Ok(()) => {}
        Err(fs::TryLockError::WouldBlock) => return Ok(None),
        Err(fs::TryLockError::Error(e)) => {
            return Err(io(format!("locking {}", lock.display()), e));
        }
    }
    // One a deletion removed after this opened it is a lock on nothing.
    Ok(lock.exists().then_some(file))
}

/// Delete the disk at `path`, and then its lock file, which the caller holds. A disk that cannot
/// be deleted keeps its lock file, so that a sweep still finds it.
fn delete(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    let _ = fs::remove_file(lock_path(path));
    Ok(())
}

fn lock_path(path: &Path) -> PathBuf {
    let mut lock = path.as_os_str().to_owned();
    lock.push(".lock");
    PathBuf::from(lock)
}

fn io(context: String, source: io::Error) -> Error {
    Error::Io { context, source }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fstree::FsTree;

    #[test]
    fn a_new_upper_is_an_empty_filesystem_held_until_it_drops() {
        let dir = tempfile::tempdir().unwrap();
        let upper = Upper::create(dir.path(), "work", 2).unwrap().unwrap();
        assert_eq!(upper.path, dir.path().join("work.ext4"));
        let read = FsTree::from_ext4(File::open(&upper.path).unwrap()).unwrap();
        let paths: Vec<_> = read.walk().into_iter().map(|(path, ..)| path).collect();
        assert_eq!(paths, [b"".to_vec(), b"lost+found".to_vec()]);

        assert!(Upper::create(dir.path(), "work", 2).unwrap().is_none());
        Upper::sweep(dir.path());
        assert!(upper.path.is_file(), "one held is not swept");

        drop(upper);
        assert!(fs::read_dir(dir.path()).unwrap().next().is_none());
        assert!(Upper::create(dir.path(), "work", 2).unwrap().is_some());
    }

    #[test]
    fn a_disk_nothing_holds_is_swept_and_not_made_again_until_then() {
        let dir = tempfile::tempdir().unwrap();
        // What a process that ended leaves: the disk, and a lock file nothing holds.
        let path = dir.path().join("left.ext4");
        crate::machine::create_upper(&path, 2).unwrap();
        fs::write(lock_path(&path), b"").unwrap();
        assert!(Upper::create(dir.path(), "left", 2).unwrap().is_none());

        Upper::sweep(dir.path());
        assert!(!path.exists());
        assert!(!lock_path(&path).exists());
    }

    #[test]
    fn a_disk_not_deleted_keeps_its_lock_file_for_the_next_sweep() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stuck.ext4");
        // What a file cannot be deleted as: a directory with something in it.
        fs::create_dir(&path).unwrap();
        fs::write(path.join("in"), b"").unwrap();
        fs::write(lock_path(&path), b"").unwrap();

        Upper::sweep(dir.path());
        assert!(lock_path(&path).is_file());

        fs::remove_dir_all(&path).unwrap();
        fs::write(&path, b"").unwrap();
        Upper::sweep(dir.path());
        assert!(!path.exists());
        assert!(!lock_path(&path).exists());
    }
}
