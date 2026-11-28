//! A sandbox's files as [`http_over_x::webdav`] serves them: each request asked of the agent in
//! its machine, over the I/O channel ([`Machine::io`]), as the guest's processes see the files.

use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use bytes::Bytes;
use http_over_x::webdav::{Entry, Filesystem, Meta};

use crate::machine::Machine;
use crate::machine::wire::{IoFailure, IoMeta, IoReq, IoResp, MAX_DATA};

/// A sandbox's files for one request, and what holds it for a conditional write.
pub(super) struct Files {
    pub(super) machine: Arc<Machine>,
    /// Each volume the sandbox mounts writable: where in the guest, and its own directory.
    pub(super) mounts: Arc<[(String, PathBuf)]>,
    /// The sandbox's hold for a conditional write, taken before its volumes'.
    pub(super) writing: Arc<tokio::sync::Mutex<()>>,
    /// Each volume it mounts writable, once, in the order of their directories: this process's
    /// hold on it, and its own directory.
    pub(super) volumes: Vec<(Arc<tokio::sync::Mutex<()>>, PathBuf)>,
}

impl Files {
    /// Ask `request` of the guest, with `data` to write, off the runtime's threads.
    async fn io(&self, request: IoReq, data: Vec<u8>) -> io::Result<(IoResp, Vec<u8>)> {
        let machine = Arc::clone(&self.machine);
        let (answer, read) = tokio::task::spawn_blocking(move || machine.io(&request, &data))
            .await
            .map_err(io::Error::other)?
            .map_err(io::Error::other)?;
        match answer {
            IoResp::Failed(failure) => Err(failed(failure)),
            answer => Ok((answer, read)),
        }
    }

    /// Ask `request`, which changes something and answers nothing but that it did.
    async fn done(&self, request: IoReq) -> io::Result<()> {
        self.io(request, Vec::new()).await.map(drop)
    }
}

impl Filesystem for Files {
    async fn stat(&self, path: &str) -> io::Result<Meta> {
        match self.io(IoReq::Stat { path: at(path) }, Vec::new()).await? {
            (IoResp::Meta(meta), _) => Ok(described(&meta)),
            _ => Err(io::Error::other(
                "the agent answered a stat with no metadata",
            )),
        }
    }

    async fn read_dir(&self, path: &str) -> io::Result<Vec<Entry>> {
        let request = IoReq::ReadDir { path: at(path) };
        let IoResp::Entries(entries) = self.io(request, Vec::new()).await?.0 else {
            return Err(io::Error::other(
                "the agent answered a listing with no entries",
            ));
        };
        Ok(entries
            .into_iter()
            .map(|(name, meta)| Entry {
                name,
                meta: described(&meta),
            })
            .collect())
    }

    async fn read(&self, path: &str, offset: u64, len: u64) -> io::Result<Bytes> {
        let request = IoReq::Read {
            path: at(path),
            offset,
            len,
        };
        Ok(Bytes::from(self.io(request, Vec::new()).await?.1))
    }

    async fn create(&self, path: &str) -> io::Result<()> {
        let request = IoReq::Write {
            path: at(path),
            offset: None,
        };
        self.done(request).await
    }

    /// A [`MAX_DATA`] at a time, the most one request to the agent carries.
    async fn write(&self, path: &str, offset: u64, data: Bytes) -> io::Result<()> {
        let mut offset = offset;
        for piece in data.chunks(MAX_DATA) {
            let request = IoReq::Write {
                path: at(path),
                offset: Some(offset),
            };
            self.io(request, piece.to_vec()).await?;
            offset += piece.len() as u64;
        }
        Ok(())
    }

    async fn create_dir(&self, path: &str) -> io::Result<()> {
        self.done(IoReq::CreateDir { path: at(path) }).await
    }

    async fn remove_file(&self, path: &str) -> io::Result<()> {
        self.done(IoReq::Remove { path: at(path) }).await
    }

    async fn remove_dir(&self, path: &str) -> io::Result<()> {
        self.done(IoReq::Remove { path: at(path) }).await
    }

    /// Keeping the mode and owner of a file at `to`: a whole write goes over what was there as
    /// it was, and a `MOVE` finds nothing there, taken away first.
    async fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        let request = IoReq::Rename {
            from: at(from),
            to: at(to),
            keep: true,
        };
        self.done(request).await
    }

    async fn copy(&self, from: &str, to: &str) -> io::Result<()> {
        let request = IoReq::Copy {
            from: at(from),
            to: at(to),
        };
        self.done(request).await
    }

    /// The sandbox, and each volume it mounts writable after it, so a write through a mount and
    /// one through the volume's own files are held against each other.
    async fn write_lock(&self) -> io::Result<Box<dyn Send>> {
        let sandbox = Arc::clone(&self.writing).lock_owned().await;
        let mut held = Vec::with_capacity(self.volumes.len());
        for (serial, dir) in &self.volumes {
            held.push(super::writing(Arc::clone(serial), dir).await?);
        }
        Ok(Box::new((sandbox, held)))
    }

    /// In a volume the sandbox mounts writable, the volume's own files on this host are asked,
    /// not the guest: the guest may still hold what it saw at the path a moment ago, before a
    /// write through the volume's files or another sandbox's moved another file there. A path
    /// those files do not [answer for](crate::Volume::answers), through a link in the volume, by
    /// a name they refuse or in a mount of the volume's own, is asked of the guest, which sees
    /// it.
    async fn stat_latest(&self, path: &str) -> io::Result<Meta> {
        if let Some((dir, within)) = mounted(&self.mounts, &at(path)) {
            let volume = crate::Volume::open(&dir).map_err(super::volume::failed)?;
            if volume.answers(&within) {
                let stat = volume.stat(&within).await;
                return stat
                    .map(|stat| super::volume::meta(&stat))
                    .map_err(super::volume::failed);
            }
        }
        self.stat(path).await
    }
}

/// The volume of `mounts` that `path`, absolute in the guest, is in: its own directory, and
/// `path` in its files. The deepest mount `path` is under, so a volume mounted in another's
/// mount is the one its paths are in. A mount's own place is the guest's, not a path in the
/// volume, and a path a symlink in the guest leads into a mount is not seen as in it.
fn mounted(mounts: &[(String, PathBuf)], path: &str) -> Option<(PathBuf, String)> {
    mounts
        .iter()
        .filter_map(|(at, dir)| Some((path.strip_prefix(at.as_str())?.strip_prefix('/')?, dir)))
        .filter(|(within, _)| !within.is_empty())
        .min_by_key(|(within, _)| within.len())
        .map(|(within, dir)| (dir.clone(), within.to_owned()))
}

/// `path`, relative to the root, as the guest spells it: absolute, and the root `/`.
fn at(path: &str) -> String {
    format!("/{path}")
}

fn described(meta: &IoMeta) -> Meta {
    Meta {
        is_dir: meta.is_dir,
        size: meta.len,
        modified: meta.modified.map(|since| UNIX_EPOCH + since),
        etag: None,
    }
}

/// The guest's failure as an [`io::Error`] of the kind WebDAV answers it by.
fn failed(failure: IoFailure) -> io::Error {
    let kind = match &failure {
        IoFailure::NotFound => io::ErrorKind::NotFound,
        IoFailure::NotADirectory => io::ErrorKind::NotADirectory,
        IoFailure::Exists => io::ErrorKind::AlreadyExists,
        IoFailure::IsADirectory => io::ErrorKind::IsADirectory,
        IoFailure::Forbidden => io::ErrorKind::PermissionDenied,
        IoFailure::Full => io::ErrorKind::StorageFull,
        IoFailure::Other(_) => io::ErrorKind::Other,
    };
    io::Error::new(kind, format!("{failure:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_is_in_the_deepest_mount_it_is_under() {
        let mounts = vec![
            ("/data".to_owned(), PathBuf::from("/home/volume/data")),
            ("/data/sub".to_owned(), PathBuf::from("/home/volume/sub")),
        ];
        let at = |path| mounted(&mounts, path);
        assert_eq!(
            at("/data/a.txt"),
            Some((PathBuf::from("/home/volume/data"), "a.txt".to_owned()))
        );
        assert_eq!(
            at("/data/dir/a.txt"),
            Some((PathBuf::from("/home/volume/data"), "dir/a.txt".to_owned()))
        );
        assert_eq!(
            at("/data/sub/a.txt"),
            Some((PathBuf::from("/home/volume/sub"), "a.txt".to_owned()))
        );
        // A name that only begins with a mount's is not in it, and a mount's own place is the
        // guest's, not a path in the volume.
        assert_eq!(at("/database/a.txt"), None);
        assert_eq!(at("/data"), None);
        assert_eq!(at("/data/"), None);
        assert_eq!(at("/tmp/a.txt"), None);
    }
}
