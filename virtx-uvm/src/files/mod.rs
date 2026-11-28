//! The files endpoints of the protocol: the files of a volume or a sandbox over WebDAV at
//! `/volumes/{name}/files/` and `/sandboxes/{id}/files/`, as [`http_over_x::webdav::serve`]
//! answers them.
//!
//! A volume's files are the directory on this host a machine shares, its mounts at their
//! paths. A sandbox's are its machine's root as its processes see it, asked of the agent there
//! ([`Machine::io`]).
//!
//! A write with `If-Match` or `If-None-Match` holds the volume or the sandbox as it lands,
//! in this process and every other over the home. A sandbox's holds each volume the sandbox
//! mounts writable too, after the sandbox and in the order of their directories, so one
//! through a mount and one through the volume's own files are held against each other; and
//! a `PUT` to a path in such a mount is checked against the volume's own files on this host as
//! it lands, not as the guest sees them: the guest may hold what it saw there for a few seconds
//! after a change made outside it.
//!
//! Where a volume is kept beyond this machine ([`Files::backed_by`]), it is brought up to what is
//! kept before a request is answered from it, and held there while a request writes it.

mod sandbox;
mod volume;

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::response::{IntoResponse, Response};
use http::{Request, StatusCode};
use http_body_util::BodyExt as _;

use crate::HostPaths;
use crate::backend::{self, Backing, Stored};
use crate::machine::Machine;
use crate::sandbox::Activity;
use crate::volume::{Error as VolumeError, Store as VolumeStore};

/// The files endpoints over one home's volumes, and its sandboxes' as each is asked of.
pub struct Files {
    volumes: VolumeStore,
    /// Each volume's hold for a conditional write, by its name, before its [lock file](writing)
    /// too, through its own files or a sandbox that mounts it. A sandbox's are held by the
    /// sandbox's own first.
    serial: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// Where the volumes are kept beyond this machine, if anywhere.
    backing: Backing,
    host_paths: HostPaths,
}

impl Files {
    /// The endpoints over `volumes`, naming none of this host's paths.
    pub fn new(volumes: VolumeStore) -> Files {
        Files {
            volumes,
            serial: Mutex::default(),
            backing: Backing::default(),
            host_paths: HostPaths::Hidden,
        }
    }

    /// These, each volume kept beyond this machine by `backing`'s backend, once it has one:
    /// [brought up](crate::StorageBackend::bring_up) before a request reads it, and
    /// [taken](crate::StorageBackend::take) before one writes it, until that is answered.
    pub fn backed_by(mut self, backing: Backing) -> Files {
        self.backing = backing;
        self
    }

    /// These endpoints, naming this host's paths as `host_paths` says.
    pub fn with_host_paths(mut self, host_paths: HostPaths) -> Files {
        self.host_paths = host_paths;
        self
    }

    /// Answer `request`, one to `/volumes/{name}/files` and what is under it, from the volume
    /// `name`: `400` for a name that cannot be one, `404` for no volume by it.
    pub async fn volume(&self, name: &str, request: Request<Body>) -> Response {
        // Held where it is kept until this is answered, for a write.
        let _taken = match self.backing.get() {
            None => None,
            Some(backend) => {
                let reads = matches!(
                    request.method().as_str(),
                    "GET" | "HEAD" | "OPTIONS" | "PROPFIND" | "PATCH"
                );
                let taken = match reads {
                    true => backend.bring_up(Stored::Volume(name)).await.map(|()| None),
                    false => backend.take(Stored::Volume(name)).await.map(Some),
                };
                match taken {
                    Ok(taken) => taken,
                    Err(e) => return kept_refused(e).into_response(),
                }
            }
        };
        let volume = match self.volumes.open(name) {
            Ok(volume) => Arc::new(volume),
            Err(e) => {
                let status = match e {
                    VolumeError::InvalidName(_) => StatusCode::BAD_REQUEST,
                    VolumeError::Unknown(_) => StatusCode::NOT_FOUND,
                    _ => StatusCode::INTERNAL_SERVER_ERROR,
                };
                let said = match &e {
                    VolumeError::Io { source, .. } => self.host_paths.told(source.kind(), &e),
                    _ => e.to_string(),
                };
                return refused(status, said).into_response();
            }
        };
        let files = volume::Files {
            volume,
            serial: self.serial(name),
        };
        let prefix = format!("/volumes/{name}/files");
        http_over_x::webdav::serve(&prefix, files, request)
            .await
            .map(Body::new)
    }

    /// Answer `request`, one to `/sandboxes/{id}/files` and what is under it, from the guest
    /// of `machine`, the sandbox's, keeping `activity` until the answer's body has been sent or
    /// dropped: a file read is sent as it is read from the guest.
    pub async fn sandbox(
        &self,
        id: &str,
        machine: Arc<Machine>,
        activity: Activity,
        request: Request<Body>,
    ) -> Response {
        let sandbox = activity.sandbox();
        let mounts: Arc<[(String, PathBuf)]> = sandbox.mounts().into();
        let mut dirs: Vec<PathBuf> = mounts.iter().map(|(_, dir)| dir.clone()).collect();
        dirs.sort();
        dirs.dedup();
        let volumes = dirs
            .into_iter()
            .map(|dir| {
                let name = dir.file_name().unwrap_or_default().to_string_lossy();
                (self.serial(&name), dir)
            })
            .collect();
        let files = sandbox::Files {
            machine,
            mounts,
            writing: Arc::clone(sandbox.writing()),
            volumes,
        };
        let prefix = format!("/sandboxes/{id}/files");
        let response = http_over_x::webdav::serve(&prefix, files, request).await;
        response.map(|body| {
            Body::new(body.map_frame(move |frame| {
                let _ = &activity;
                frame
            }))
        })
    }

    /// The hold of the volume `name` for a conditional write, in this process.
    fn serial(&self, name: &str) -> Arc<tokio::sync::Mutex<()>> {
        Arc::clone(
            self.serial
                .lock()
                .unwrap()
                .entry(name.to_owned())
                .or_default(),
        )
    }
}

/// Hold a volume, whose own directory is `dir`, for one conditional write, waiting while
/// another holds it, in this process or another over the home: first `serial`, this process's
/// hold on it, then its `writing` file, locked, which the system lets go of with a process that
/// ends. The file is tried, not waited on, so a write waiting holds no thread, and one whose
/// client is gone stops waiting. The file is let go of before this process's hold, so the next
/// here finds it free.
async fn writing(
    serial: Arc<tokio::sync::Mutex<()>>,
    dir: &Path,
) -> io::Result<(std::fs::File, tokio::sync::OwnedMutexGuard<()>)> {
    let held = serial.lock_owned().await;
    let file = tokio::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("writing"))
        .await?
        .into_std()
        .await;
    let mut wait = std::time::Duration::from_millis(2);
    loop {
        match file.try_lock() {
            Ok(()) => return Ok((file, held)),
            Err(std::fs::TryLockError::WouldBlock) => {
                tokio::time::sleep(wait).await;
                wait = (wait * 2).min(std::time::Duration::from_millis(100));
            }
            Err(std::fs::TryLockError::Error(e)) => return Err(e),
        }
    }
}

/// A request refused: the status, and a line saying why.
pub(crate) struct Refusal(StatusCode, String);

impl IntoResponse for Refusal {
    fn into_response(self) -> Response {
        (self.0, self.1).into_response()
    }
}

/// `status`, saying why.
fn refused(status: StatusCode, why: impl fmt::Display) -> Refusal {
    Refusal(status, why.to_string())
}

/// What a request on a volume's files is refused with when its backend refuses it with `e`: a
/// hold elsewhere as a lock is, `423`. Why a backend failed can name what it runs on, so it goes
/// to the server's stderr instead.
fn kept_refused(e: backend::Refusal) -> Refusal {
    match e {
        backend::Refusal::InUse(_) => refused(StatusCode::LOCKED, e),
        backend::Refusal::Exists(_) => refused(StatusCode::CONFLICT, e),
        backend::Refusal::Unknown(_) => refused(StatusCode::NOT_FOUND, e),
        backend::Refusal::Failed(e) => {
            eprintln!("the storage backend failed: {e:#}");
            refused(
                StatusCode::INTERNAL_SERVER_ERROR,
                "the server's storage failed",
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use http::{Method, header};

    use super::*;

    const PREFIX: &str = "/volumes/v/files";

    #[tokio::test]
    async fn a_put_answers_the_etag_of_the_file_it_wrote() {
        let scratch = tempfile::tempdir().unwrap();
        let volumes = VolumeStore::new(scratch.path().join("volume")).unwrap();
        volumes.create("v").unwrap();
        let files = Files::new(volumes);
        let sent =
            async |method: Method, path: &str, if_match: Option<&str>, body: &'static str| {
                let mut request = Request::builder().method(method).uri(path);
                if let Some(etag) = if_match {
                    request = request.header(header::IF_MATCH, etag);
                }
                let response = files
                    .volume("v", request.body(Body::from(body)).unwrap())
                    .await;
                let etag = response.headers().get(header::ETAG);
                (
                    response.status(),
                    etag.map(|etag| etag.to_str().unwrap().to_owned()),
                )
            };

        let path = "/volumes/v/files/a.txt";
        let (status, put) = sent(Method::PUT, path, None, "one").await;
        assert_eq!(status, StatusCode::CREATED, "{path}");
        let put = put.expect("a PUT answers an ETag");
        let (status, got) = sent(Method::GET, path, None, "").await;
        assert_eq!(status, StatusCode::OK, "{path}");
        assert_eq!(got.as_deref(), Some(put.as_str()), "{path}");

        // The tag a write was answered is the one the next write is taken over. That write
        // is longer: a tag is a file's length and time, and on a host whose file times move a
        // clock tick at a time two writes in a row can have one time.
        let (status, again) = sent(Method::PUT, path, Some(&put), "three").await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{path}");
        let (_, got) = sent(Method::GET, path, None, "").await;
        assert_eq!(got, again, "{path}");
        let (status, _) = sent(Method::PUT, path, Some(&put), "old").await;
        assert_eq!(status, StatusCode::PRECONDITION_FAILED, "{path}");
    }

    #[tokio::test]
    async fn a_volume_that_does_not_open_is_refused_by_the_kind_of_its_failure() {
        let scratch = tempfile::tempdir().unwrap();
        let volumes = VolumeStore::new(scratch.path()).unwrap();
        let table = volumes.create("v").unwrap().path().join("mounts.json");
        std::fs::write(&table, "not a table").unwrap();
        let account = format!("reading {}", table.display());
        for (host_paths, named) in [(HostPaths::Hidden, false), (HostPaths::Named, true)] {
            let files = Files::new(volumes.clone()).with_host_paths(host_paths);
            let got = files.volume("v", Request::get(PREFIX).body(Body::empty()).unwrap());
            let response = got.await;
            assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
            let body = response.into_body().collect().await.unwrap().to_bytes();
            let body = String::from_utf8(body.to_vec()).unwrap();
            assert_eq!(body.starts_with(&account), named, "{host_paths:?}: {body}");
            if !named {
                assert_eq!(body, "the server's files: other error");
            }
        }
    }
}
