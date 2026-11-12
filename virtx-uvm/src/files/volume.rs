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

use super::Condition;
use crate::volume::{DirentKind, Error, Stat, Volume};

/// How much a `GET` reads at a time: each read opens the file again.
pub(super) const READ: usize = 1 << 20;

#[derive(Clone)]
pub(super) struct Fs {
    pub(super) volume: Arc<Volume>,
    /// The request's condition, if it has one, which a whole write checks again as it lands.
    pub(super) condition: Option<Arc<Condition>>,
}

/// A file open for reading, or for writing from its start.
struct File {
    volume: Arc<Volume>,
    path: String,
    /// Where a whole write goes until it is flushed: a name beside `path`, renamed over it
    /// then, and removed if this drops first. So nothing is at `path` but what was there
    /// until all of the write has arrived.
    saving: Option<String>,
    /// For a whole write with a condition: the condition, and what was at `path` when it was
    /// checked, `None` for nothing, which must be there still when the write lands.
    checked: Option<(Arc<Condition>, Option<Meta>)>,
    offset: u64,
    meta: Meta,
}

#[derive(Clone, Debug, PartialEq)]
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
            let checked = match (&self.condition, options.write && options.truncate) {
                (Some(condition), true) => Some((Arc::clone(condition), there.clone())),
                _ => None,
            };
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
                saving,
                checked,
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
            self.volume
                .write(
                    self.saving.as_deref().unwrap_or(&self.path),
                    &buf,
                    Some(self.offset),
                )
                .map_err(|e| refused(&e))?;
            self.offset += buf.len() as u64;
            self.meta.len = self.meta.len.max(self.offset);
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
            let offset = match pos {
                SeekFrom::Start(n) => Some(n),
                SeekFrom::End(n) => self.meta.len.checked_add_signed(n),
                SeekFrom::Current(n) => self.offset.checked_add_signed(n),
            };
            self.offset = offset.ok_or(FsError::GeneralFailure)?;
            Ok(self.offset)
        })
    }

    /// A whole write, all here now, goes over the path; with a condition, only over what the
    /// condition was checked against.
    fn flush(&mut self) -> FsFuture<'_, ()> {
        Box::pin(async move {
            let Some(saving) = &self.saving else {
                return Ok(());
            };
            match &self.checked {
                None => self
                    .volume
                    .rename(saving, &self.path, true)
                    .map_err(|e| refused(&e))?,
                Some((condition, there)) => {
                    let landed = self
                        .volume
                        .rename_if(saving, &self.path, |now| now.map(described) == *there)
                        .map_err(|e| refused(&e))?;
                    if !landed {
                        condition.broke();
                        return Err(FsError::GeneralFailure);
                    }
                }
            }
            self.saving = None;
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

fn described(stat: &Stat) -> Meta {
    Meta {
        is_dir: stat.kind == DirentKind::Dir,
        len: stat.size,
        modified: stat.mtime,
    }
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
            condition: None,
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

    /// Two whole writes with a condition, each opened over one file before either lands: the
    /// first to land does, and the other is refused as it lands and marks its condition broken,
    /// leaving nothing behind. Over a file there, as `If-Match` writes, and where nothing is, as
    /// `If-None-Match: *` does.
    #[tokio::test]
    async fn of_two_conditional_writes_opened_over_one_file_the_first_to_land_does() {
        let scratch = tempfile::tempdir().unwrap();
        let store = Store::new(scratch.path().join("volumes")).unwrap();
        let volume = Arc::new(store.create("v").unwrap());
        let files = volume.files();
        volume.write("there", b"old", None).unwrap();
        for (name, options) in [
            (
                "there",
                OpenOptions {
                    write: true,
                    create: true,
                    truncate: true,
                    ..Default::default()
                },
            ),
            (
                "new",
                OpenOptions {
                    write: true,
                    create: true,
                    create_new: true,
                    truncate: true,
                    ..Default::default()
                },
            ),
        ] {
            let (one, two) = (Arc::new(Condition::default()), Arc::default());
            let fs = |condition: &Arc<Condition>| Fs {
                volume: Arc::clone(&volume),
                condition: Some(Arc::clone(condition)),
            };
            let path = DavPath::new(&format!("/{name}")).unwrap();
            let mut first = fs(&one).open(&path, options.clone()).await.unwrap();
            let mut second = fs(&two).open(&path, options.clone()).await.unwrap();
            first
                .write_bytes(Bytes::from_static(b"first"))
                .await
                .unwrap();
            second
                .write_bytes(Bytes::from_static(b"second"))
                .await
                .unwrap();

            second.flush().await.unwrap();
            let refused = first.flush().await;
            assert!(
                matches!(refused, Err(FsError::GeneralFailure)),
                "{name}: {refused:?}"
            );
            assert!(one.0.load(std::sync::atomic::Ordering::Relaxed), "{name}");
            assert!(!two.0.load(std::sync::atomic::Ordering::Relaxed), "{name}");
            drop((first, second));
            assert_eq!(volume.read(name, 0, None).await.unwrap(), b"second");
        }
        assert_eq!(names(&files), ["new", "there"]);
    }
}
