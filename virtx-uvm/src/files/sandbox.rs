//! A sandbox's files as `dav-server` reads and writes them: each request asked of the agent in
//! its machine, over the I/O channel ([`Machine::io`]), as the guest's processes see the files.

use std::io::SeekFrom;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::{Buf, Bytes};
use dav_server::davpath::DavPath;
use dav_server::fs::{
    DavDirEntry, DavFile, DavFileSystem, DavMetaData, FsError, FsFuture, FsStream, OpenOptions,
    ReadDirMeta,
};
use futures_util::stream;

use super::volume::Meta;
use crate::machine::Machine;
use crate::machine::wire::{IoFailure, IoMeta, IoReq, IoResp, MAX_DATA};

#[derive(Clone)]
pub(super) struct Fs(pub(super) Arc<Machine>);

/// A file open for reading, or for writing from its start.
struct File {
    machine: Arc<Machine>,
    path: String,
    /// Where a whole write goes until it is flushed: a name beside `path`, renamed over it
    /// then, and removed if this drops first. So nothing is at `path` but what was there
    /// until all of the write has arrived.
    saving: Option<String>,
    /// What has arrived and is not yet written, sent a [`MAX_DATA`] at a time.
    pending: Vec<u8>,
    offset: u64,
    meta: Meta,
}

struct Entry {
    name: Vec<u8>,
    meta: Meta,
}

impl std::fmt::Debug for File {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{} in a sandbox", self.path)
    }
}

/// Ask `request` of `machine`'s guest, with `data` to write, off the runtime's threads.
async fn io(
    machine: &Arc<Machine>,
    request: IoReq,
    data: Vec<u8>,
) -> Result<(IoResp, Vec<u8>), FsError> {
    let machine = Arc::clone(machine);
    let (answer, read) = tokio::task::spawn_blocking(move || machine.io(&request, &data))
        .await
        .map_err(|_| FsError::GeneralFailure)?
        .map_err(|_| FsError::GeneralFailure)?;
    match answer {
        IoResp::Failed(failure) => Err(refused(&failure)),
        answer => Ok((answer, read)),
    }
}

/// Ask `request`, which changes something and answers nothing but that it did.
async fn done(machine: &Arc<Machine>, request: IoReq) -> Result<(), FsError> {
    io(machine, request, Vec::new()).await.map(drop)
}

async fn stat(machine: &Arc<Machine>, path: &str) -> Result<Meta, FsError> {
    let request = IoReq::Stat { path: path.into() };
    match io(machine, request, Vec::new()).await? {
        (IoResp::Meta(meta), _) => Ok(described(&meta)),
        _ => Err(FsError::GeneralFailure),
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
            let there = match stat(&self.0, &path).await {
                Ok(meta) => Some(meta),
                Err(FsError::NotFound) => None,
                Err(e) => return Err(e),
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
                    let request = IoReq::Write {
                        path: saving.clone().unwrap_or_else(|| path.clone()),
                        offset: (!options.truncate).then_some(0),
                    };
                    done(&self.0, request).await?;
                    Meta {
                        is_dir: false,
                        len: 0,
                        modified: Some(SystemTime::now()),
                    }
                }
            };
            Ok(Box::new(File {
                machine: Arc::clone(&self.0),
                path,
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
            let request = IoReq::ReadDir { path: at(path) };
            let IoResp::Entries(entries) = io(&self.0, request, Vec::new()).await?.0 else {
                return Err(FsError::GeneralFailure);
            };
            let entries: Vec<_> = entries
                .into_iter()
                .map(|(name, meta)| {
                    Ok(Box::new(Entry {
                        name: name.into_bytes(),
                        meta: described(&meta),
                    }) as Box<dyn DavDirEntry>)
                })
                .collect();
            Ok(Box::pin(stream::iter(entries)) as FsStream<Box<dyn DavDirEntry>>)
        })
    }

    fn metadata<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, Box<dyn DavMetaData>> {
        Box::pin(
            async move { Ok(Box::new(stat(&self.0, &at(path)).await?) as Box<dyn DavMetaData>) },
        )
    }

    fn create_dir<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, ()> {
        Box::pin(async move { done(&self.0, IoReq::CreateDir { path: at(path) }).await })
    }

    fn remove_dir<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, ()> {
        Box::pin(async move { done(&self.0, IoReq::Remove { path: at(path) }).await })
    }

    fn remove_file<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, ()> {
        Box::pin(async move { done(&self.0, IoReq::Remove { path: at(path) }).await })
    }

    /// What was at `to` is gone already: `dav-server` takes it away first, or refuses the
    /// request when told not to overwrite.
    fn rename<'a>(&'a self, from: &'a DavPath, to: &'a DavPath) -> FsFuture<'a, ()> {
        Box::pin(async move {
            let request = IoReq::Rename {
                from: at(from),
                to: at(to),
                keep: false,
            };
            done(&self.0, request).await
        })
    }

    /// One file, as `dav-server` copies a directory file by file.
    fn copy<'a>(&'a self, from: &'a DavPath, to: &'a DavPath) -> FsFuture<'a, ()> {
        Box::pin(async move {
            let request = IoReq::Copy {
                from: at(from),
                to: at(to),
            };
            done(&self.0, request).await
        })
    }
}

impl File {
    /// Write what is pending, a [`MAX_DATA`] at a time, leaving up to `keep` of it to come.
    async fn send(&mut self, keep: usize) -> Result<(), FsError> {
        while self.pending.len() > keep {
            let rest = self.pending.split_off(self.pending.len().min(MAX_DATA));
            let data = std::mem::replace(&mut self.pending, rest);
            let len = data.len() as u64;
            let request = IoReq::Write {
                path: self.saving.clone().unwrap_or_else(|| self.path.clone()),
                offset: Some(self.offset),
            };
            io(&self.machine, request, data).await?;
            self.offset += len;
        }
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

    /// Held until a [`MAX_DATA`] has arrived, so a body's many small pieces go to the guest as
    /// few requests.
    fn write_bytes(&mut self, buf: Bytes) -> FsFuture<'_, ()> {
        Box::pin(async move {
            self.pending.extend_from_slice(&buf);
            self.meta.len = self.meta.len.max(self.offset + self.pending.len() as u64);
            self.send(MAX_DATA - 1).await
        })
    }

    fn read_bytes(&mut self, count: usize) -> FsFuture<'_, Bytes> {
        Box::pin(async move {
            let request = IoReq::Read {
                path: self.path.clone(),
                offset: self.offset,
                len: count as u64,
            };
            let (_, read) = io(&self.machine, request, Vec::new()).await?;
            self.offset += read.len() as u64;
            Ok(Bytes::from(read))
        })
    }

    fn seek(&mut self, pos: SeekFrom) -> FsFuture<'_, u64> {
        Box::pin(async move {
            self.send(0).await?;
            let offset = match pos {
                SeekFrom::Start(n) => Some(n),
                SeekFrom::End(n) => self.meta.len.checked_add_signed(n),
                SeekFrom::Current(n) => self.offset.checked_add_signed(n),
            };
            self.offset = offset.ok_or(FsError::GeneralFailure)?;
            Ok(self.offset)
        })
    }

    /// A whole write, all here now, goes over the path, as what was there was.
    fn flush(&mut self) -> FsFuture<'_, ()> {
        Box::pin(async move {
            self.send(0).await?;
            if let Some(saving) = self.saving.take() {
                let request = IoReq::Rename {
                    from: saving.clone(),
                    to: self.path.clone(),
                    keep: true,
                };
                if let Err(e) = done(&self.machine, request).await {
                    self.saving = Some(saving);
                    return Err(e);
                }
            }
            Ok(())
        })
    }
}

impl Drop for File {
    fn drop(&mut self) {
        if let Some(saving) = self.saving.take() {
            let machine = Arc::clone(&self.machine);
            std::thread::spawn(move || machine.io(&IoReq::Remove { path: saving }, &[]));
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

/// `path` as the guest spells it: absolute, and the root `/`.
fn at(path: &DavPath) -> String {
    format!("/{}", super::volume::at(path))
}

/// A name beside `path`, in its directory, for a write on its way to `path`: hidden, and not
/// one a second write to `path` at the same time gets.
fn beside(path: &str) -> String {
    let (dir, name) = path.rsplit_once('/').unwrap_or(("", path));
    format!("{dir}/.{name}.{}.saving", uuid::Uuid::new_v4().simple())
}

fn described(meta: &IoMeta) -> Meta {
    Meta {
        is_dir: meta.is_dir,
        len: meta.len,
        modified: meta.modified.map(|since| UNIX_EPOCH + since),
    }
}

/// What `dav-server` is told of the guest's failure, which it answers with the status RFC 4918
/// gives: a parent that is no directory is `NotFound`, which a `PUT` or `MKCOL` turns into
/// `409`.
fn refused(failure: &IoFailure) -> FsError {
    match failure {
        IoFailure::NotFound | IoFailure::NotADirectory => FsError::NotFound,
        IoFailure::Exists => FsError::Exists,
        IoFailure::IsADirectory | IoFailure::Forbidden => FsError::Forbidden,
        IoFailure::Full => FsError::InsufficientStorage,
        IoFailure::Other(_) => FsError::GeneralFailure,
    }
}
