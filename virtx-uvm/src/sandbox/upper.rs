//! The upper disks sandboxes write over their images to, one ext4 each, kept in one directory
//! under the sandboxes' ids.
//!
//! ```text
//! <dir>/<id>.ext4        the disk
//! <dir>/<id>.ext4.lock   what the sandbox holds for as long as it is in a process
//! <dir>/<id>.json        its record, for one kept past its process (see `record`)
//! ```
//!
//! A writable ext4 takes one machine at a time, and nothing stops a second from opening it;
//! the lock is the agreement that does. It is on a file beside the disk, not on the disk: on
//! Windows a lock is mandatory, and one on the disk would refuse the machine's own reads and
//! writes. A sandbox's process holds it from before its disk is made until the sandbox is
//! deleted or the process lets it go, so a disk being made, or of a sandbox another process
//! has, is never taken for one left behind.

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};

use super::Error;
use super::record::Record;

/// An upper disk, held from before it was made until this drops: which deletes it, unless it is
/// [kept](Upper::kept) -- or until the process holding it ends, which leaves it to
/// [`Upper::sweep`], or, kept, to the next process over its directory.
#[derive(Debug)]
pub(super) struct Upper {
    pub(super) id: String,
    pub(super) path: PathBuf,
    /// Whether it outlasts this: its record written, so that dropping it lets it go and leaves
    /// it, with its record, for a process started again over its directory.
    pub(super) kept: bool,
    _lock: File,
}

impl Upper {
    /// Make an empty disk under `dir` for sandbox `id`, that may grow to `gib` GiB, or `None`
    /// for an id another holds, has made or keeps.
    pub(super) fn create(dir: &Path, id: &str, gib: u32) -> Result<Option<Upper>, Error> {
        fs::create_dir_all(dir).map_err(|e| io(format!("making {}", dir.display()), e))?;
        let path = dir.join(format!("{id}.ext4"));
        let Some(lock) = hold(&path)? else {
            return Ok(None);
        };
        if path.exists() || Record::path(dir, id).exists() {
            return Ok(None);
        }
        let upper = Upper {
            id: id.to_owned(),
            path,
            kept: false,
            _lock: lock,
        };
        // Dropped on a failure, so the lock goes with a disk that was not made.
        crate::machine::create_upper(&upper.path, gib)?;
        Ok(Some(upper))
    }

    /// The disk of each sandbox kept under `dir` that no process holds, held now. A record whose
    /// disk is gone is deleted.
    pub(super) fn kept(dir: &Path) -> Vec<Upper> {
        ids(dir)
            .into_iter()
            .filter_map(|id| Upper::kept_one(dir, &id))
            .collect()
    }

    /// The disk of sandbox `id`, kept under `dir` with its record, if no process holds it, held
    /// now.
    pub(super) fn kept_one(dir: &Path, id: &str) -> Option<Upper> {
        if !Record::path(dir, id).exists() {
            return None;
        }
        let path = dir.join(format!("{id}.ext4"));
        let Ok(Some(lock)) = hold(&path) else {
            return None;
        };
        let upper = Upper {
            kept: path.is_file(),
            id: id.to_owned(),
            path,
            _lock: lock,
        };
        upper.kept.then_some(upper)
    }

    /// Delete what sandboxes whose process ended without deleting them left under `dir`, that
    /// nothing holds: each disk with no record, each record with no disk, each record half
    /// written, and each lock file with no disk beside it. A disk with its record is kept.
    pub(super) fn sweep(dir: &Path) {
        for id in ids(dir) {
            let path = dir.join(format!("{id}.ext4"));
            if let Ok(Some(lock)) = hold(&path) {
                let upper = Upper {
                    kept: path.is_file() && Record::path(dir, &id).exists(),
                    id,
                    path,
                    _lock: lock,
                };
                let _ = fs::remove_file(Record::part(dir, &upper.id));
                drop(upper);
            }
        }
    }

    /// Whether a sandbox of any process lives under `dir`: one holds a disk there, or one is
    /// kept there for a process started again. True where `dir` cannot be read, since what is
    /// in it is then not known.
    pub(super) fn lives(dir: &Path) -> bool {
        let entries = match fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(e) => return e.kind() != io::ErrorKind::NotFound,
        };
        entries.flatten().any(|entry| {
            let path = entry.path();
            let name = path.to_string_lossy();
            if name.ends_with(".json") {
                return true;
            }
            if !name.ends_with(".ext4.lock") {
                return false;
            }
            File::open(&path)
                .is_ok_and(|file| matches!(file.try_lock(), Err(fs::TryLockError::WouldBlock)))
        })
    }
}

impl Drop for Upper {
    fn drop(&mut self) {
        if !self.kept {
            let _ = delete(&self.path);
        }
    }
}

/// The id of each sandbox under `dir` anything is left of: a disk, its lock or its record.
fn ids(dir: &Path) -> BTreeSet<String> {
    let Ok(entries) = fs::read_dir(dir) else {
        return BTreeSet::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            [".ext4", ".ext4.lock", ".json", ".json.part"]
                .iter()
                .find_map(|suffix| name.strip_suffix(suffix))
                .map(str::to_owned)
        })
        .collect()
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

/// Delete the record of the disk at `path`, the disk, and then its lock file, which the caller
/// holds. A disk that cannot be deleted keeps its lock file, so that a sweep still finds it.
fn delete(path: &Path) -> io::Result<()> {
    if let (Some(dir), Some(id)) = (path.parent(), path.file_stem()) {
        let id = id.to_string_lossy();
        let _ = fs::remove_file(Record::path(dir, &id));
        let _ = fs::remove_file(Record::part(dir, &id));
    }
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    // What the embedder keeps beside it goes with it ([`Disks::forget`](crate::disks::Disks)).
    let forgotten = crate::disks::disks().forget(path);
    let _ = fs::remove_file(lock_path(path));
    forgotten
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
        assert!(Upper::lives(dir.path()));

        drop(upper);
        assert!(fs::read_dir(dir.path()).unwrap().next().is_none());
        assert!(!Upper::lives(dir.path()));
        assert!(!Upper::lives(&dir.path().join("none")));
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
        assert!(
            !Upper::lives(dir.path()),
            "one nothing holds is no sandbox's"
        );

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

    #[test]
    fn a_lock_file_nothing_holds_without_a_disk_is_swept() {
        let dir = tempfile::tempdir().unwrap();
        // What a process that ended before making its disk leaves.
        let lock = lock_path(&dir.path().join("early.ext4"));
        fs::write(&lock, b"").unwrap();
        let held = Upper::create(dir.path(), "held", 2).unwrap().unwrap();
        fs::remove_file(&held.path).unwrap();

        Upper::sweep(dir.path());
        assert!(!lock.exists());
        assert!(lock_path(&held.path).is_file(), "one held is not swept");
    }

    /// A record, as a sandbox kept past its process has one.
    fn record(dir: &Path, id: &str) {
        let args = virtx_core::SandboxCreateArgs::new("alpine:3.20");
        let record = Record {
            args,
            keys: Default::default(),
            exited: false,
            jobs: Vec::new(),
        };
        record.write(dir, id).unwrap();
    }

    #[test]
    fn a_kept_disk_outlasts_its_upper_and_the_sweep_and_is_taken_again_once() {
        let dir = tempfile::tempdir().unwrap();
        let mut upper = Upper::create(dir.path(), "kept", 2).unwrap().unwrap();
        record(dir.path(), "kept");
        upper.kept = true;
        let path = upper.path.clone();
        assert!(Upper::kept(dir.path()).is_empty(), "one held is not taken");
        drop(upper);
        assert!(path.is_file());
        assert!(
            Upper::lives(dir.path()),
            "a kept one lives with nothing holding it"
        );

        Upper::sweep(dir.path());
        assert!(path.is_file(), "a kept one is not swept");
        assert!(
            Upper::create(dir.path(), "kept", 2).unwrap().is_none(),
            "its id is not made again"
        );
        let mut taken = Upper::kept(dir.path());
        assert_eq!(taken.len(), 1);
        assert_eq!((taken[0].id.as_str(), taken[0].kept), ("kept", true));
        assert!(Upper::kept(dir.path()).is_empty(), "taken once");

        // Deleted, it goes with its record.
        taken[0].kept = false;
        drop(taken);
        assert!(fs::read_dir(dir.path()).unwrap().next().is_none());
    }

    #[test]
    fn a_disk_with_no_record_and_a_record_with_no_disk_are_swept_and_one_being_made_is_not() {
        let dir = tempfile::tempdir().unwrap();
        // A making cut short before its record was written: its disk alone, which nothing holds.
        let cut = dir.path().join("cut.ext4");
        crate::machine::create_upper(&cut, 2).unwrap();
        // A sandbox whose disk is gone: its record alone, and a record half written.
        record(dir.path(), "gone");
        fs::write(Record::part(dir.path(), "half"), b"{").unwrap();
        // A sandbox another process is making: its disk, held, with no record yet.
        let making = Upper::create(dir.path(), "making", 2).unwrap().unwrap();

        assert!(Upper::kept(dir.path()).is_empty());
        assert!(!Record::path(dir.path(), "gone").exists());
        record(dir.path(), "gone");
        Upper::sweep(dir.path());
        let mut left: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(left, ["making.ext4", "making.ext4.lock"]);
        drop(making);
    }
}
