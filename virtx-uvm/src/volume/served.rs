//! A [`Remote`] served to a guest over virtio-fs, as libkrun's [`ExternalFs`].
//!
//! The guest names files by inode and a volume by path, so this keeps the table between them:
//! an inode for every path the guest has looked up and not yet forgotten, inode 1 the root.
//! libkrun calls in from its own threads, which block on a runtime of this one's.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::UNIX_EPOCH;

use krun::{ExternalAttr, ExternalEntry, ExternalFs};

use super::{DirentKind, Remote, Stat};

const ROOT: u64 = 1;

pub struct Served {
    volume: Arc<Remote>,
    runtime: tokio::runtime::Runtime,
    inodes: Mutex<Inodes>,
}

/// Each inode the guest holds: its path, and how many lookups it has not forgotten.
struct Inodes {
    paths: HashMap<u64, (PathBuf, u64)>,
    by_path: HashMap<PathBuf, u64>,
    next: u64,
}

impl Served {
    pub fn new(volume: Arc<Remote>) -> io::Result<Served> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?;
        let root = PathBuf::from("/");
        Ok(Served {
            volume,
            runtime,
            inodes: Mutex::new(Inodes {
                paths: HashMap::from([(ROOT, (root.clone(), 1))]),
                by_path: HashMap::from([(root, ROOT)]),
                next: ROOT + 1,
            }),
        })
    }

    fn path(&self, ino: u64) -> io::Result<PathBuf> {
        let inodes = self.inodes.lock().unwrap();
        inodes
            .paths
            .get(&ino)
            .map(|(path, _)| path.clone())
            .ok_or_else(|| errno(libc::ENOENT))
    }

    fn stat(&self, path: &Path) -> io::Result<Stat> {
        self.runtime.block_on(self.volume.stat(path)).map_err(host)
    }
}

impl ExternalFs for Served {
    fn lookup(&self, parent: u64, name: &[u8]) -> io::Result<ExternalAttr> {
        let name = std::str::from_utf8(name).map_err(|_| errno(libc::ENOENT))?;
        let parent = self.path(parent)?;
        let path = match name {
            "." => parent,
            ".." => parent.parent().unwrap_or(&parent).to_path_buf(),
            _ if name.contains('/') => return Err(errno(libc::ENOENT)),
            _ => parent.join(name),
        };
        let stat = self.stat(&path)?;
        let mut inodes = self.inodes.lock().unwrap();
        let ino = match inodes.by_path.get(&path) {
            Some(&ino) => ino,
            None => {
                let ino = inodes.next;
                inodes.next += 1;
                inodes.by_path.insert(path.clone(), ino);
                inodes.paths.insert(ino, (path, 0));
                ino
            }
        };
        inodes.paths.get_mut(&ino).expect("just held").1 += 1;
        Ok(attr(ino, &stat))
    }

    fn forget(&self, ino: u64, count: u64) {
        if ino == ROOT {
            return;
        }
        let mut inodes = self.inodes.lock().unwrap();
        let Some((path, held)) = inodes.paths.get_mut(&ino) else {
            return;
        };
        *held = held.saturating_sub(count);
        if *held == 0 {
            let path = path.clone();
            inodes.paths.remove(&ino);
            inodes.by_path.remove(&path);
        }
    }

    fn getattr(&self, ino: u64) -> io::Result<ExternalAttr> {
        Ok(attr(ino, &self.stat(&self.path(ino)?)?))
    }

    fn readdir(&self, ino: u64, offset: u64) -> io::Result<Vec<ExternalEntry>> {
        let path = self.path(ino)?;
        let listed = self
            .runtime
            .block_on(self.volume.list(&path))
            .map_err(host)?;
        let inodes = self.inodes.lock().unwrap();
        Ok(listed
            .into_iter()
            .enumerate()
            .skip(offset as usize)
            .map(|(at, entry)| ExternalEntry {
                // An inode the guest holds keeps its number; any other is only a hint, which a
                // lookup replaces.
                ino: inodes
                    .by_path
                    .get(&path.join(&entry.name))
                    .copied()
                    .unwrap_or(u64::MAX),
                dir: entry.kind == DirentKind::Dir,
                name: entry.name.into_bytes(),
                offset: at as u64 + 1,
            })
            .collect())
    }

    fn read(&self, ino: u64, offset: u64, size: u32) -> io::Result<Vec<u8>> {
        let path = self.path(ino)?;
        let mut data = vec![0; size as usize];
        let mut filled = 0;
        while filled < data.len() {
            let read = self
                .runtime
                .block_on(
                    self.volume
                        .read_at(&path, &mut data[filled..], offset + filled as u64),
                )
                .map_err(host)?;
            if read == 0 {
                break;
            }
            filled += read;
        }
        data.truncate(filled);
        Ok(data)
    }
}

fn attr(ino: u64, stat: &Stat) -> ExternalAttr {
    ExternalAttr {
        ino,
        dir: stat.kind == DirentKind::Dir,
        size: stat.size,
        modified: stat
            .mtime
            .and_then(|at| at.duration_since(UNIX_EPOCH).ok())
            .unwrap_or_default(),
    }
}

fn errno(code: i32) -> io::Error {
    io::Error::from_raw_os_error(code)
}

/// `e` with the host errno libkrun passes on to the guest.
fn host(e: io::Error) -> io::Error {
    if e.raw_os_error().is_some() {
        return e;
    }
    errno(match e.kind() {
        io::ErrorKind::NotFound => libc::ENOENT,
        io::ErrorKind::NotADirectory => libc::ENOTDIR,
        io::ErrorKind::IsADirectory => libc::EISDIR,
        io::ErrorKind::PermissionDenied => libc::EACCES,
        io::ErrorKind::ReadOnlyFilesystem => libc::EROFS,
        _ => libc::EIO,
    })
}
