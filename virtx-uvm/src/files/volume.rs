//! A [`Volume`] as `dav-server` reads and writes one: each request to a path is the
//! volume's own, mounts and all, and a path's spelling is the volume's.

use std::io::{self, SeekFrom};
use std::sync::Arc;
use std::time::SystemTime;

use bytes::{Buf, Bytes};
use dav_server::davpath::DavPath;
use dav_server::fs::{
    DavDirEntry, DavFile, DavFileSystem, DavMetaData, FsError, FsFuture, FsStream, OpenOptions,
    ReadDirMeta,
};
use futures_util::stream;

use super::Landing;
use crate::volume::{DirentKind, Error, Stat, Volume};

/// How much a `GET` reads at a time: each read opens the file again.
pub(super) const READ: usize = 1 << 20;

/// How much of a body is gathered before it is written: each write opens the file again, and a
/// body comes in pieces far smaller.
const WRITE: usize = 256 << 10;

/// A volume's files for one request, with the [`Landing`] of a conditional `PUT`.
#[derive(Clone)]
pub(super) struct Fs {
    pub(super) volume: Arc<Volume>,
    pub(super) landing: Option<Arc<Landing>>,
}

/// A file open for reading, or for writing from its start.
struct File {
    volume: Arc<Volume>,
    path: String,
    /// Where a whole write goes until it is flushed: a name beside `path`, renamed over it
    /// then, and removed if this drops first. So nothing is at `path` but what was there
    /// until all of the write has arrived.
    saving: Option<String>,
    /// What a whole write is checked by as it lands, with a condition.
    landing: Option<Arc<Landing>>,
    /// What has arrived and is not yet written, written a [`WRITE`] at a time.
    pending: Vec<u8>,
    offset: u64,
    meta: Meta,
}

#[derive(Clone, Debug)]
pub(super) struct Meta {
    pub(super) is_dir: bool,
    pub(super) len: u64,
    pub(super) modified: Option<SystemTime>,
}

struct Entry {
    name: Vec<u8>,
    meta: Meta,
}

impl std::fmt::Debug for File {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{} in volume {}", self.path, self.volume.name())
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
            let there = match self.volume.stat(&path).await {
                Ok(stat) => Some(described(&stat)),
                Err(e) if refused(&e) == FsError::NotFound => None,
                Err(e) => return Err(refused(&e)),
            };
            if there.as_ref().is_some_and(|meta| meta.is_dir) {
                return Err(FsError::Forbidden);
            }
            let mut saving = None;
            let meta = match (options.write, there) {
                (false, Some(meta)) => meta,
                (false, None) => return Err(FsError::NotFound),
                (true, Some(_)) if options.create_new => return Err(FsError::Exists),
                (true, None) if !options.create => return Err(FsError::NotFound),
                (true, _) => {
                    // Made now, beside the path or, written in place, at it: what a `PUT`
                    // under a missing directory is refused for is told before any of the
                    // body is read.
                    if options.truncate {
                        saving = Some(beside(&path));
                    }
                    self.volume
                        .write(
                            saving.as_deref().unwrap_or(&path),
                            &[],
                            (!options.truncate).then_some(0),
                        )
                        .map_err(|e| refused(&e))?;
                    Meta {
                        is_dir: false,
                        len: 0,
                        modified: Some(SystemTime::now()),
                    }
                }
            };
            Ok(Box::new(File {
                volume: Arc::clone(&self.volume),
                path,
                landing: saving.as_ref().and(self.landing.clone()),
                saving,
                pending: Vec::new(),
                offset: 0,
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
            let entries = self
                .volume
                .read_dir(&at(path))
                .await
                .map_err(|e| refused(&e))?;
            let entries: Vec<_> = entries
                .iter()
                .filter_map(|entry| {
                    let stat = entry.stat()?;
                    Some(Ok(Box::new(Entry {
                        name: entry.name.clone().into_bytes(),
                        meta: described(stat),
                    }) as Box<dyn DavDirEntry>))
                })
                .collect();
            Ok(Box::pin(stream::iter(entries)) as FsStream<Box<dyn DavDirEntry>>)
        })
    }

    /// The same as `metadata`: a link is nothing this root answers.
    fn symlink_metadata<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, Box<dyn DavMetaData>> {
        self.metadata(path)
    }

    fn metadata<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, Box<dyn DavMetaData>> {
        Box::pin(async move {
            let stat = self.volume.stat(&at(path)).await.map_err(|e| refused(&e))?;
            Ok(Box::new(described(&stat)) as Box<dyn DavMetaData>)
        })
    }

    fn create_dir<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, ()> {
        Box::pin(async move {
            self.volume
                .create_dir(&at(path), false)
                .map_err(|e| refused(&e))
        })
    }

    fn remove_dir<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, ()> {
        Box::pin(async move {
            self.volume
                .remove_file(&at(path), true)
                .map_err(|e| refused(&e))
        })
    }

    fn remove_file<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, ()> {
        Box::pin(async move {
            self.volume
                .remove_file(&at(path), false)
                .map_err(|e| refused(&e))
        })
    }

    /// What was at `to` is gone already: `dav-server` takes it away first, or refuses the
    /// request when told not to overwrite.
    fn rename<'a>(&'a self, from: &'a DavPath, to: &'a DavPath) -> FsFuture<'a, ()> {
        Box::pin(async move {
            self.volume
                .rename(&at(from), &at(to), true)
                .map_err(|e| refused(&e))
        })
    }

    /// One file, as `dav-server` copies a directory file by file.
    fn copy<'a>(&'a self, from: &'a DavPath, to: &'a DavPath) -> FsFuture<'a, ()> {
        Box::pin(async move {
            self.volume
                .copy(&at(from), &at(to), true)
                .await
                .map_err(|e| refused(&e))
        })
    }
}

impl File {
    /// Write what is pending at the offset.
    fn send(&mut self) -> Result<(), FsError> {
        if self.pending.is_empty() {
            return Ok(());
        }
        self.volume
            .write(
                self.saving.as_deref().unwrap_or(&self.path),
                &self.pending,
                Some(self.offset),
            )
            .map_err(|e| refused(&e))?;
        self.offset += self.pending.len() as u64;
        self.pending.clear();
        Ok(())
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

    /// Held until a [`WRITE`] has arrived, so a body's many small pieces go to the file as few
    /// writes.
    fn write_bytes(&mut self, buf: Bytes) -> FsFuture<'_, ()> {
        Box::pin(async move {
            self.pending.extend_from_slice(&buf);
            self.meta.len = self.meta.len.max(self.offset + self.pending.len() as u64);
            if self.pending.len() >= WRITE {
                self.send()?;
            }
            Ok(())
        })
    }

    fn read_bytes(&mut self, count: usize) -> FsFuture<'_, Bytes> {
        Box::pin(async move {
            let read = self
                .volume
                .read(&self.path, self.offset, Some(count as u64))
                .await
                .map_err(|e| refused(&e))?;
            self.offset += read.len() as u64;
            Ok(Bytes::from(read))
        })
    }

    fn seek(&mut self, pos: SeekFrom) -> FsFuture<'_, u64> {
        Box::pin(async move {
            self.send()?;
            let offset = match pos {
                SeekFrom::Start(n) => Some(n),
                SeekFrom::End(n) => self.meta.len.checked_add_signed(n),
                SeekFrom::Current(n) => self.offset.checked_add_signed(n),
            };
            self.offset = offset.ok_or(FsError::GeneralFailure)?;
            Ok(self.offset)
        })
    }

    /// A whole write, all here now, goes over the path: with a condition, as its [`Landing`]
    /// lets it, once all of it is in the file, so the hold is only for the check and the move.
    /// Its metadata is read then, before it goes over the path, as a rename keeps a file's length
    /// and modification time, so a `PUT` answers the `ETag` of the file it wrote and not of one
    /// another write puts at the path right after.
    fn flush(&mut self) -> FsFuture<'_, ()> {
        Box::pin(async move {
            self.send()?;
            let at = self.saving.as_deref().unwrap_or(&self.path);
            let read = match self.volume.stat(at).await {
                Ok(stat) => Some(described(&stat)),
                Err(e) => {
                    log::debug!(
                        "{self:?}: the written file was not read, so the PUT answers no ETag or \
                         Last-Modified: {e}"
                    );
                    None
                }
            };
            if let Some(saving) = self.saving.clone() {
                let (volume, path) = (Arc::clone(&self.volume), self.path.clone());
                let land =
                    async move { volume.rename(&saving, &path, true).map_err(|e| refused(&e)) };
                match &self.landing {
                    None => land.await?,
                    Some(landing) => {
                        let (volume, path) = (Arc::clone(&self.volume), self.path.clone());
                        let there = async move { there(&volume, &path).await };
                        landing.land(there, land).await?
                    }
                }
                self.saving = None;
            }
            self.meta = written(read, &self.meta);
            Ok(())
        })
    }
}

impl Drop for File {
    fn drop(&mut self) {
        if let Some(saving) = &self.saving {
            let _ = self.volume.remove_file(saving, false);
        }
    }
}

impl DavDirEntry for Entry {
    fn name(&self) -> Vec<u8> {
        self.name.clone()
    }

    fn metadata(&self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        Box::pin(async move { Ok(Box::new(self.meta.clone()) as Box<dyn DavMetaData>) })
    }
}

impl DavMetaData for Meta {
    fn len(&self) -> u64 {
        self.len
    }

    /// Never: a link is nothing these roots answer.
    fn is_symlink(&self) -> bool {
        false
    }

    fn modified(&self) -> Result<SystemTime, FsError> {
        self.modified.ok_or(FsError::NotImplemented)
    }

    fn is_dir(&self) -> bool {
        self.is_dir
    }
}

/// `path` as the volume spells it: relative, and the root empty.
pub(super) fn at(path: &DavPath) -> String {
    String::from_utf8_lossy(path.as_bytes())
        .trim_matches('/')
        .to_owned()
}

/// A name beside `path`, in its directory, for a write on its way to `path`: hidden, and not
/// one a second write to `path` at the same time gets.
fn beside(path: &str) -> String {
    let (dir, name) = path.rsplit_once('/').unwrap_or(("", path));
    let id = uuid::Uuid::new_v4().simple();
    match dir {
        "" => format!(".{name}.{id}.saving"),
        dir => format!("{dir}/.{name}.{id}.saving"),
    }
}

/// What is at `path` in `volume` now, or nothing: what a conditional write is checked against
/// as it lands.
pub(super) async fn there(volume: &Volume, path: &str) -> Result<Option<Meta>, FsError> {
    match volume.stat(path).await {
        Ok(stat) => Ok(Some(described(&stat))),
        Err(e) if refused(&e) == FsError::NotFound => Ok(None),
        Err(e) => Err(refused(&e)),
    }
}

fn described(stat: &Stat) -> Meta {
    Meta {
        is_dir: stat.kind == DirentKind::Dir,
        len: stat.size,
        modified: stat.mtime,
    }
}

/// The metadata of a file just written: `read` of it, or if it could not be read, `meta` with no
/// time, so the write is answered with no validator rather than one the file does not have.
pub(super) fn written(read: Option<Meta>, meta: &Meta) -> Meta {
    read.unwrap_or_else(|| Meta {
        modified: None,
        ..meta.clone()
    })
}

/// What `dav-server` is told of a volume's failure, which it answers with the status
/// RFC 4918 gives: a parent that is no directory is `NotFound`, which a `PUT` or `MKCOL`
/// turns into `409`, and a change a mount will not take is `Forbidden`.
fn refused(e: &Error) -> FsError {
    match e {
        Error::Unknown(_) | Error::NoMount { .. } => FsError::NotFound,
        Error::AlreadyExists(_) | Error::MountExists(_) => FsError::Exists,
        Error::InvalidName(_) | Error::InvalidPath(_) | Error::Refused(_) | Error::InUse(_) => {
            FsError::Forbidden
        }
        Error::Io { source, .. } => match source.kind() {
            io::ErrorKind::NotFound | io::ErrorKind::NotADirectory => FsError::NotFound,
            io::ErrorKind::AlreadyExists => FsError::Exists,
            io::ErrorKind::PermissionDenied
            | io::ErrorKind::ReadOnlyFilesystem
            | io::ErrorKind::IsADirectory
            | io::ErrorKind::Unsupported => FsError::Forbidden,
            io::ErrorKind::StorageFull => FsError::InsufficientStorage,
            _ => FsError::GeneralFailure,
        },
        Error::Source(_) | Error::Origins(..) => FsError::GeneralFailure,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::volume::Store;

    /// What is in the directory `dir`, by name.
    fn names(dir: &std::path::Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[tokio::test]
    async fn a_whole_write_lands_at_once_or_not_at_all() {
        let scratch = tempfile::tempdir().unwrap();
        let store = Store::new(scratch.path().join("volumes")).unwrap();
        let fs = Fs {
            volume: Arc::new(store.create("v").unwrap()),
            landing: None,
        };
        let files = fs.volume.files();
        fs.volume.create_dir("dir", false).unwrap();
        fs.volume.write("dir/a.txt", b"old", None).unwrap();
        let whole = || OpenOptions {
            write: true,
            create: true,
            truncate: true,
            ..Default::default()
        };
        let path = DavPath::new("/dir/a.txt").unwrap();

        // Dropped before it is flushed: what was there stays, and nothing else is left.
        let mut file = fs.open(&path, whole()).await.unwrap();
        file.write_bytes(Bytes::from_static(b"half")).await.unwrap();
        assert_eq!(fs.volume.read("dir/a.txt", 0, None).await.unwrap(), b"old");
        assert_eq!(names(&files.join("dir")).len(), 2);
        drop(file);
        assert_eq!(names(&files.join("dir")), ["a.txt"]);

        let mut file = fs.open(&path, whole()).await.unwrap();
        file.write_bytes(Bytes::from_static(b"new")).await.unwrap();
        file.flush().await.unwrap();
        drop(file);
        assert_eq!(fs.volume.read("dir/a.txt", 0, None).await.unwrap(), b"new");
        assert_eq!(names(&files.join("dir")), ["a.txt"]);

        // At the root, and new.
        let mut file = fs
            .open(&DavPath::new("/b.txt").unwrap(), whole())
            .await
            .unwrap();
        file.write_bytes(Bytes::from_static(b"b")).await.unwrap();
        file.flush().await.unwrap();
        drop(file);
        assert_eq!(names(&files), ["b.txt", "dir"]);

        // Under a missing directory: refused before any of the body, nothing left behind.
        let refused = fs.open(&DavPath::new("/no/c.txt").unwrap(), whole()).await;
        assert!(matches!(refused, Err(FsError::NotFound)), "{refused:?}");
        assert_eq!(names(&files), ["b.txt", "dir"]);
    }

    /// A body that comes in many small pieces, more than a [`WRITE`] of them, lands whole and
    /// in order: what is gathered is written as it fills, and the rest when the file is flushed.
    /// Until then, what was at the path is what is there.
    #[tokio::test]
    async fn a_body_in_many_pieces_lands_whole_and_in_order() {
        let scratch = tempfile::tempdir().unwrap();
        let store = Store::new(scratch.path().join("volumes")).unwrap();
        let fs = Fs {
            volume: Arc::new(store.create("v").unwrap()),
            landing: None,
        };
        fs.volume.write("a.bin", b"old", None).unwrap();
        let whole = OpenOptions {
            write: true,
            create: true,
            truncate: true,
            ..Default::default()
        };
        let path = DavPath::new("/a.bin").unwrap();
        let body: Vec<u8> = (0..(3 * WRITE + 1234)).map(|i| (i % 251) as u8).collect();

        let mut file = fs.open(&path, whole).await.unwrap();
        for piece in body.chunks(4000) {
            file.write_bytes(Bytes::copy_from_slice(piece))
                .await
                .unwrap();
        }
        assert_eq!(fs.volume.read("a.bin", 0, None).await.unwrap(), b"old");
        file.flush().await.unwrap();
        drop(file);
        assert_eq!(fs.volume.read("a.bin", 0, None).await.unwrap(), body);
    }
}
