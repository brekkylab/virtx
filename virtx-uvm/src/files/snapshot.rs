//! A snapshot's files as `dav-server` reads and writes them: the ext4 image's tree, read
//! whole for a request and written back whole if the request changed it.
//!
//! What a client sees is the image's `upper` directory, where the guest mounts a machine's
//! writable layer in overlayfs's spelling: what a machine wrote, and nothing the image below
//! shipped. So a directory is there only once a machine, or a request here, made it, and a
//! file is made with what the guest's root would own. What a client cannot use -- a deletion,
//! a link, a device -- is not seen, and a file put at its path replaces it. The rest of the
//! image, overlayfs's work directory among it, is never seen.

use std::fs;
use std::io::{self, BufReader, SeekFrom};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::{Buf, Bytes};
use dav_server::davpath::DavPath;
use dav_server::fs::{
    DavDirEntry, DavFile, DavFileSystem, DavMetaData, FsError, FsFuture, FsStream, OpenOptions,
    ReadDirMeta,
};
use futures_util::stream;
use tokio::sync::OwnedMutexGuard;

use super::volume::Meta;
use crate::fstree::{Entry, FsTree, Meta as TreeMeta, Timestamp};
use crate::snapshot::{Error, Lock, Snapshot};

/// A snapshot read whole, held against every other use until it is saved or dropped.
pub(super) struct Image {
    snapshot: Snapshot,
    /// The image's ceiling, which the one written back keeps.
    size: u64,
    tree: Arc<Mutex<FsTree>>,
    dirty: Arc<AtomicBool>,
    _lock: Lock,
    _serial: OwnedMutexGuard<()>,
}

/// The tree, shared with the handler for one request.
#[derive(Clone)]
pub(super) struct Fs {
    tree: Arc<Mutex<FsTree>>,
    dirty: Arc<AtomicBool>,
}

/// A file read from the tree, or written into it once whole.
#[derive(Debug)]
struct File {
    fs: Fs,
    path: Vec<u8>,
    data: Vec<u8>,
    offset: u64,
    writing: bool,
    meta: Meta,
}

struct DirEntry {
    name: Vec<u8>,
    meta: Meta,
}

impl std::fmt::Debug for Fs {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("a snapshot's tree")
    }
}

impl Image {
    /// `snapshot` read whole, once `serial` is this request's: refused as `InUse` while a
    /// machine or a removal holds it.
    pub(super) async fn load(
        snapshot: Snapshot,
        serial: OwnedMutexGuard<()>,
    ) -> Result<Image, Error> {
        let lock = snapshot.lock()?;
        let path = snapshot.path.clone();
        let (tree, size) = tokio::task::spawn_blocking(move || {
            let file = fs::File::open(&path).map_err(|e| Error::Io {
                context: format!("opening {}", path.display()),
                source: e,
            })?;
            let size = file
                .metadata()
                .map_err(|e| Error::Io {
                    context: format!("reading {}", path.display()),
                    source: e,
                })?
                .len();
            Ok::<_, Error>((FsTree::from_ext4(BufReader::new(file))?, size))
        })
        .await
        .map_err(|e| Error::Io {
            context: "reading the snapshot".into(),
            source: io::Error::other(e),
        })??;
        // A fresh image has no upper directory yet; the guest makes it at its first boot.
        let mut tree = tree;
        if !matches!(tree.entry(UPPER), Ok(Some(Entry::Directory { .. }))) {
            let _ = tree.insert(UPPER, directory());
        }
        Ok(Image {
            snapshot,
            size,
            tree: Arc::new(Mutex::new(tree)),
            dirty: Arc::new(AtomicBool::new(false)),
            _lock: lock,
            _serial: serial,
        })
    }

    pub(super) fn fs(&self) -> Fs {
        Fs {
            tree: Arc::clone(&self.tree),
            dirty: Arc::clone(&self.dirty),
        }
    }

    /// Write the tree back if anything changed it: a new image beside the disk, moved over
    /// it once whole, so a failure leaves the disk as it was.
    pub(super) async fn save(self) -> Result<(), Error> {
        if !self.dirty.load(Ordering::Relaxed) {
            return Ok(());
        }
        let tree = std::mem::take(&mut *self.tree.lock().unwrap());
        let path = self.snapshot.path.clone();
        let size = self.size;
        tokio::task::spawn_blocking(move || {
            let part = path.with_extension("ext4.saving");
            let _ = fs::remove_file(&part);
            let written = fs::File::create_new(&part)
                .map_err(|e| Error::Io {
                    context: format!("creating {}", part.display()),
                    source: e,
                })
                .and_then(|file| tree.to_ext4(file, size).map_err(Error::from))
                .and_then(|()| {
                    fs::rename(&part, &path).map_err(|e| Error::Io {
                        context: format!("moving {} into place", path.display()),
                        source: e,
                    })
                });
            if written.is_err() {
                let _ = fs::remove_file(&part);
            }
            written
        })
        .await
        .map_err(|e| Error::Io {
            context: "writing the snapshot".into(),
            source: io::Error::other(e),
        })?
    }
}

impl Fs {
    /// What `dav-server` sees at `path`: a file or a directory, or nothing.
    fn seen(&self, path: &[u8]) -> Result<Option<Meta>, FsError> {
        let tree = self.tree.lock().unwrap();
        Ok(tree
            .entry(path)
            .map_err(|_| FsError::Forbidden)?
            .and_then(described))
    }

    /// Refused unless the directory `path` goes in is there.
    fn in_dir(&self, path: &[u8]) -> Result<(), FsError> {
        let (parent, _) = split(path);
        match self.seen(parent)? {
            Some(meta) if meta.is_dir => Ok(()),
            _ => Err(FsError::NotFound),
        }
    }

    fn insert(&self, path: &[u8], entry: Entry) -> Result<(), FsError> {
        self.tree
            .lock()
            .unwrap()
            .insert(path, entry)
            .map_err(|_| FsError::Forbidden)?;
        self.dirty.store(true, Ordering::Relaxed);
        Ok(())
    }
}

impl DavFileSystem for Fs {
    fn open<'a>(
        &'a self,
        path: &'a DavPath,
        options: OpenOptions,
    ) -> FsFuture<'a, Box<dyn DavFile>> {
        Box::pin(async move {
            let path = at(path);
            let there = self.seen(&path)?;
            if there.as_ref().is_some_and(|meta| meta.is_dir) {
                return Err(FsError::Forbidden);
            }
            let (data, meta) = match (options.write, there) {
                (false, Some(meta)) => {
                    let tree = self.tree.lock().unwrap();
                    let data = match tree.entry(&path).map_err(|_| FsError::Forbidden)? {
                        Some(Entry::File { data, .. }) => data.clone(),
                        _ => return Err(FsError::NotFound),
                    };
                    (data, meta)
                }
                (false, None) => return Err(FsError::NotFound),
                (true, Some(_)) if options.create_new => return Err(FsError::Exists),
                (true, None) if !options.create => return Err(FsError::NotFound),
                (true, _) => {
                    self.in_dir(&path)?;
                    (
                        Vec::new(),
                        Meta {
                            is_dir: false,
                            len: 0,
                            modified: Some(SystemTime::now()),
                        },
                    )
                }
            };
            Ok(Box::new(File {
                fs: self.clone(),
                path,
                data,
                offset: 0,
                writing: options.write,
                meta,
            }) as Box<dyn DavFile>)
        })
    }

    fn read_dir<'a>(
        &'a self,
        path: &'a DavPath,
        _: ReadDirMeta,
    ) -> FsFuture<'a, FsStream<Box<dyn DavDirEntry>>> {
        Box::pin(async move {
            let tree = self.tree.lock().unwrap();
            let entries = tree
                .entries(&at(path))
                .map_err(|_| FsError::Forbidden)?
                .ok_or(FsError::NotFound)?;
            let entries: Vec<_> = entries
                .into_iter()
                .filter_map(|(name, entry)| {
                    let meta = described(entry)?;
                    Some(Ok(Box::new(DirEntry {
                        name: name.to_vec(),
                        meta,
                    }) as Box<dyn DavDirEntry>))
                })
                .collect();
            Ok(Box::pin(stream::iter(entries)) as FsStream<Box<dyn DavDirEntry>>)
        })
    }

    fn metadata<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, Box<dyn DavMetaData>> {
        Box::pin(async move {
            let meta = self.seen(&at(path))?.ok_or(FsError::NotFound)?;
            Ok(Box::new(meta) as Box<dyn DavMetaData>)
        })
    }

    fn create_dir<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, ()> {
        Box::pin(async move {
            let path = at(path);
            if path.is_empty() || self.seen(&path)?.is_some() {
                return Err(FsError::Exists);
            }
            self.in_dir(&path)?;
            self.insert(&path, directory())
        })
    }

    fn remove_dir<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, ()> {
        self.remove_file(path)
    }

    fn remove_file<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, ()> {
        Box::pin(async move {
            let path = at(path);
            if path == UPPER {
                return Err(FsError::Forbidden);
            }
            self.seen(&path)?.ok_or(FsError::NotFound)?;
            self.tree
                .lock()
                .unwrap()
                .prune(&path)
                .map_err(|_| FsError::Forbidden)?;
            self.dirty.store(true, Ordering::Relaxed);
            Ok(())
        })
    }

    fn rename<'a>(&'a self, from: &'a DavPath, to: &'a DavPath) -> FsFuture<'a, ()> {
        Box::pin(async move {
            let (from, to) = (at(from), at(to));
            if from == UPPER || to == UPPER {
                return Err(FsError::Forbidden);
            }
            self.seen(&from)?.ok_or(FsError::NotFound)?;
            let moved = self
                .tree
                .lock()
                .unwrap()
                .rename(&from, &to)
                .map_err(|_| FsError::Forbidden)?;
            if !moved {
                return Err(FsError::NotFound);
            }
            self.dirty.store(true, Ordering::Relaxed);
            Ok(())
        })
    }

    /// One file, as `dav-server` copies a directory file by file.
    fn copy<'a>(&'a self, from: &'a DavPath, to: &'a DavPath) -> FsFuture<'a, ()> {
        Box::pin(async move {
            let (from, to) = (at(from), at(to));
            let copied = {
                let tree = self.tree.lock().unwrap();
                match tree.entry(&from).map_err(|_| FsError::Forbidden)? {
                    Some(Entry::File { data, .. }) => file(data.clone()),
                    Some(_) | None => return Err(FsError::NotFound),
                }
            };
            self.in_dir(&to)?;
            self.insert(&to, copied)
        })
    }
}

impl DavFile for File {
    fn metadata(&mut self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        Box::pin(async move { Ok(Box::new(self.meta.clone()) as Box<dyn DavMetaData>) })
    }

    fn write_buf(&mut self, mut buf: Box<dyn Buf + Send>) -> FsFuture<'_, ()> {
        let bytes = buf.copy_to_bytes(buf.remaining());
        self.write_bytes(bytes)
    }

    fn write_bytes(&mut self, buf: Bytes) -> FsFuture<'_, ()> {
        Box::pin(async move {
            if !self.writing {
                return Err(FsError::Forbidden);
            }
            let end = self.offset as usize + buf.len();
            if self.data.len() < end {
                self.data.resize(end, 0);
            }
            self.data[self.offset as usize..end].copy_from_slice(&buf);
            self.offset = end as u64;
            self.meta.len = self.data.len() as u64;
            Ok(())
        })
    }

    fn read_bytes(&mut self, count: usize) -> FsFuture<'_, Bytes> {
        Box::pin(async move {
            let start = (self.offset as usize).min(self.data.len());
            let end = (start + count).min(self.data.len());
            self.offset = end as u64;
            Ok(Bytes::copy_from_slice(&self.data[start..end]))
        })
    }

    fn seek(&mut self, pos: SeekFrom) -> FsFuture<'_, u64> {
        Box::pin(async move {
            let offset = match pos {
                SeekFrom::Start(n) => Some(n),
                SeekFrom::End(n) => (self.data.len() as u64).checked_add_signed(n),
                SeekFrom::Current(n) => self.offset.checked_add_signed(n),
            };
            self.offset = offset.ok_or(FsError::GeneralFailure)?;
            Ok(self.offset)
        })
    }

    /// A written file goes into the tree whole, here, as `dav-server` ends a `PUT`.
    fn flush(&mut self) -> FsFuture<'_, ()> {
        Box::pin(async move {
            if self.writing {
                self.fs
                    .insert(&self.path, file(std::mem::take(&mut self.data)))?;
                self.writing = false;
            }
            Ok(())
        })
    }
}

impl DavDirEntry for DirEntry {
    fn name(&self) -> Vec<u8> {
        self.name.clone()
    }

    fn metadata(&self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        Box::pin(async move { Ok(Box::new(self.meta.clone()) as Box<dyn DavMetaData>) })
    }
}

/// Where the guest keeps a machine's writes in the image: overlayfs's upper directory.
const UPPER: &[u8] = b"upper";

/// `path` as the tree spells it: under the upper directory.
fn at(path: &DavPath) -> Vec<u8> {
    let bytes = path.as_bytes();
    let start = bytes.iter().take_while(|b| **b == b'/').count();
    let end = bytes.len() - bytes.iter().rev().take_while(|b| **b == b'/').count();
    in_upper(&bytes[start..end.max(start)])
}

/// A client's `path`, relative with the root empty, as the tree spells it.
pub(super) fn in_upper(path: &[u8]) -> Vec<u8> {
    if path.is_empty() {
        UPPER.to_vec()
    } else {
        [UPPER, b"/", path].concat()
    }
}

/// The directory `path` is in, and its name there.
fn split(path: &[u8]) -> (&[u8], &[u8]) {
    match path.iter().rposition(|b| *b == b'/') {
        Some(slash) => (&path[..slash], &path[slash + 1..]),
        None => (b"", path),
    }
}

/// What a client sees of `entry`: a file or a directory, and nothing else.
fn described(entry: &Entry) -> Option<Meta> {
    let (is_dir, len, meta) = match entry {
        Entry::File { meta, data } => (false, data.len() as u64, meta),
        Entry::Directory { meta, .. } => (true, 0, meta),
        _ => return None,
    };
    let modified = u64::try_from(meta.mtime.secs)
        .ok()
        .map(|secs| UNIX_EPOCH + std::time::Duration::new(secs, meta.mtime.nanos));
    Some(Meta {
        is_dir,
        len,
        modified,
    })
}

/// What a file or directory made now is: the guest's root's, as a machine makes them.
fn made(mode: u16) -> TreeMeta {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| Timestamp {
            secs: since.as_secs() as i64,
            nanos: since.subsec_nanos(),
        })
        .unwrap_or(Timestamp::ZERO);
    TreeMeta {
        mode,
        uid: 0,
        gid: 0,
        atime: now,
        mtime: now,
        ctime: now,
        crtime: now,
        xattrs: Default::default(),
    }
}

fn file(data: Vec<u8>) -> Entry {
    Entry::File {
        meta: made(0o644),
        data,
    }
}

fn directory() -> Entry {
    Entry::Directory {
        meta: made(0o755),
        opaque: false,
    }
}
