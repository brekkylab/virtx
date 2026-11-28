//! A [`Volume`]'s files as [`http_over_x::webdav`] serves them: each path the volume's own,
//! mounts and all, and a path's spelling the volume's.

use std::io;
use std::sync::Arc;

use bytes::Bytes;
use http_over_x::webdav::{Entry, Filesystem, Meta};

use crate::volume::{DirentKind, Error, Stat, Volume};

/// A volume's files for one request, and what holds it for a conditional write.
pub(super) struct Files {
    pub(super) volume: Arc<Volume>,
    /// This process's hold on the volume, before its `writing` file ([`super::writing`]).
    pub(super) serial: Arc<tokio::sync::Mutex<()>>,
}

impl Filesystem for Files {
    async fn stat(&self, path: &str) -> io::Result<Meta> {
        self.volume
            .stat(path)
            .await
            .map(|stat| meta(&stat))
            .map_err(failed)
    }

    async fn read_dir(&self, path: &str) -> io::Result<Vec<Entry>> {
        let entries = self.volume.read_dir(path).await.map_err(failed)?;
        Ok(entries
            .iter()
            .filter_map(|entry| {
                Some(Entry {
                    name: entry.name.clone(),
                    meta: meta(entry.stat()?),
                })
            })
            .collect())
    }

    async fn read(&self, path: &str, offset: u64, len: u64) -> io::Result<Bytes> {
        let read = self.volume.read(path, offset, Some(len)).await;
        read.map(Bytes::from).map_err(failed)
    }

    async fn create(&self, path: &str) -> io::Result<()> {
        self.volume.write(path, &[], None).map_err(failed)
    }

    async fn write(&self, path: &str, offset: u64, data: Bytes) -> io::Result<()> {
        self.volume.write(path, &data, Some(offset)).map_err(failed)
    }

    async fn create_dir(&self, path: &str) -> io::Result<()> {
        self.volume.create_dir(path, false).map_err(failed)
    }

    async fn remove_file(&self, path: &str) -> io::Result<()> {
        self.volume.remove_file(path, false).map_err(failed)
    }

    async fn remove_dir(&self, path: &str) -> io::Result<()> {
        self.volume.remove_file(path, true).map_err(failed)
    }

    async fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        self.volume.rename(from, to, true).map_err(failed)
    }

    async fn copy(&self, from: &str, to: &str) -> io::Result<()> {
        self.volume.copy(from, to, true).await.map_err(failed)
    }

    /// The volume, in this process and every other over the home.
    async fn write_lock(&self) -> io::Result<Box<dyn Send>> {
        let dir = self.volume.path().to_owned();
        let held = super::writing(Arc::clone(&self.serial), &dir).await?;
        Ok(Box::new(held))
    }
}

pub(super) fn meta(stat: &Stat) -> Meta {
    Meta {
        is_dir: stat.kind == DirentKind::Dir,
        size: stat.size,
        modified: stat.mtime,
        etag: None,
    }
}

/// A volume's failure as an [`io::Error`] of the kind WebDAV answers it by: a parent that is no
/// directory is `NotFound`, which a `PUT` or `MKCOL` turns into `409`, and a change a mount
/// will not take is `PermissionDenied`, `403`.
pub(super) fn failed(e: Error) -> io::Error {
    let kind = match &e {
        Error::Unknown(_) | Error::NoMount { .. } => io::ErrorKind::NotFound,
        Error::AlreadyExists(_) | Error::MountExists(_) => io::ErrorKind::AlreadyExists,
        Error::InvalidName(_) | Error::InvalidPath(_) | Error::Refused(_) | Error::InUse(_) => {
            io::ErrorKind::PermissionDenied
        }
        Error::Io { source, .. } => source.kind(),
        Error::Source(_) | Error::Origins(..) => io::ErrorKind::Other,
    };
    io::Error::new(kind, e)
}
