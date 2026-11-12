//! The files endpoints of `virtx-core`'s `PROTOCOL.md`: the files of a volume, a snapshot or a
//! sandbox over WebDAV at `/volumes/{name}/files/`, `/snapshots/{name}/files/` and
//! `/sandboxes/{id}/files/`.
//!
//! A volume's files are the directory on this host a machine shares, its mounts at their
//! paths. A snapshot's are the ext4 image a machine writes: read whole into an
//! [`FsTree`](crate::FsTree) for each request, and written back whole after one that changed
//! it, so a snapshot's requests go one at a time. A snapshot in use -- a sandbox's, or under a
//! machine -- answers `423` to everything. A sandbox's are its machine's root as its processes
//! see it, asked of the agent there ([`Machine::io`]).
//!
//! A write with `If-Match` or `If-None-Match` to a volume or a sandbox is answered in one step
//! once its body has all arrived: the body is read to its end first, holding nothing, and then,
//! holding the volume or the sandbox, `dav-server` checks the condition and writes. `dav-server` checks a condition
//! before it reads the body and moves a write into place only after, so two that checked one
//! state would both write; held across both, the second is checked against what the first
//! wrote. Read first, a slow or stalled sender holds up no other writer.

mod sandbox;
mod snapshot;
mod volume;

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::response::{IntoResponse, Response};
use dav_server::DavHandler;
use http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt as _;

use crate::machine::Machine;
use crate::sandbox::Activity;
use crate::snapshot::{Error as SnapshotError, Store as SnapshotStore};
use crate::volume::{Error as VolumeError, Store as VolumeStore};

/// Whether a sandbox holds the snapshot named, which no lock on its disk says: a sandbox
/// holds its snapshot from its making to its removal, and a machine's lock only while the
/// machine runs.
pub type Held<'a> = &'a (dyn Fn(&str) -> bool + Sync);

/// The files endpoints over one home's snapshots and volumes.
pub struct Files {
    snapshots: SnapshotStore,
    volumes: VolumeStore,
    /// Each snapshot's requests one at a time, by name, since each reads or writes it whole;
    /// and each sandbox's [conditional writes](conditional), by `sandbox:` and its id, and each
    /// volume's, by `volume:` and its name, before its [lock file](Files::writing) too.
    serial: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

/// Whose files a request names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Root {
    Volume(String),
    Snapshot(String),
}

impl fmt::Display for Root {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Root::Volume(name) => write!(f, "/volumes/{name}/files"),
            Root::Snapshot(name) => write!(f, "/snapshots/{name}/files"),
        }
    }
}

impl Files {
    /// The endpoints over `snapshots` and `volumes`.
    pub fn new(snapshots: SnapshotStore, volumes: VolumeStore) -> Files {
        Files {
            snapshots,
            volumes,
            serial: Mutex::default(),
        }
    }

    /// Answer `request`, one to `/volumes/{name}/files` or `/snapshots/{name}/files` and what is under
    /// them; anything else is `404`.
    pub async fn handle(&self, request: Request<Body>, held: Held<'_>) -> Response {
        let path = request.uri().path().to_owned();
        let root = match (
            path.strip_prefix("/volumes/"),
            path.strip_prefix("/snapshots/"),
        ) {
            (Some(rest), _) => Root::Volume(name(rest)),
            (_, Some(rest)) => Root::Snapshot(name(rest)),
            _ => return StatusCode::NOT_FOUND.into_response(),
        };
        self.dav(&root, request, held).await
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
        let handler = match handler(format!("/sandboxes/{id}/files"), &request) {
            Ok(handler) => handler,
            Err(refusal) => return refusal.into_response(),
        };
        let (request, _writing) = match conditional(&request) {
            true => {
                let request = match spooled(request).await {
                    Ok(request) => request,
                    Err(refusal) => return refusal.into_response(),
                };
                (
                    request,
                    Some(self.serial(format!("sandbox:{id}")).lock_owned().await),
                )
            }
            false => (request, None),
        };
        let handler = handler
            .filesystem(Box::new(sandbox::Fs(machine)))
            .build_handler();
        handler.handle(request).await.map(|body| {
            Body::new(body.map_frame(move |frame| {
                let _ = &activity;
                frame
            }))
        })
    }

    /// What holds the requests under `key` one at a time.
    fn serial(&self, key: String) -> Arc<tokio::sync::Mutex<()>> {
        Arc::clone(self.serial.lock().unwrap().entry(key).or_default())
    }

    /// Hold the volume `name`, whose own directory is `dir`, for one conditional write, waiting
    /// while another holds it, in this process or another over the home: first in this
    /// process, then its `writing` file, locked, which the system lets go of with a process that
    /// ends. The file is tried, not waited on, so a write waiting holds no thread, and one whose
    /// client is gone stops waiting. Held only once the write's body has all arrived.
    async fn writing(
        &self,
        name: &str,
        dir: &std::path::Path,
    ) -> io::Result<(tokio::sync::OwnedMutexGuard<()>, std::fs::File)> {
        let held = self.serial(format!("volume:{name}")).lock_owned().await;
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
                Ok(()) => return Ok((held, file)),
                Err(std::fs::TryLockError::WouldBlock) => {
                    tokio::time::sleep(wait).await;
                    wait = (wait * 2).min(std::time::Duration::from_millis(100));
                }
                Err(std::fs::TryLockError::Error(e)) => return Err(e),
            }
        }
    }

    /// A WebDAV request to `root`, a volume or a snapshot.
    async fn dav(&self, root: &Root, request: Request<Body>, held: Held<'_>) -> Response {
        let handler = match handler(root.to_string(), &request) {
            Ok(handler) => handler,
            Err(refusal) => return refusal.into_response(),
        };
        match root {
            Root::Volume(name) => {
                let volume = match self.volumes.open(name) {
                    Ok(volume) => Arc::new(volume),
                    Err(e) => return refused(status(&e), e).into_response(),
                };
                let (request, _writing) = match conditional(&request) {
                    true => {
                        let request = match spooled(request).await {
                            Ok(request) => request,
                            Err(refusal) => return refusal.into_response(),
                        };
                        match self.writing(name, volume.path()).await {
                            Ok(held) => (request, Some(held)),
                            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                                let gone = VolumeError::Unknown(name.clone());
                                return refused(status(&gone), gone).into_response();
                            }
                            Err(e) => {
                                let why = format!("holding {name} for a write: {e}");
                                return refused(StatusCode::INTERNAL_SERVER_ERROR, why)
                                    .into_response();
                            }
                        }
                    }
                    false => (request, None),
                };
                let handler = handler
                    .filesystem(Box::new(volume::Fs(volume)))
                    .build_handler();
                handler.handle(request).await.map(Body::new)
            }
            Root::Snapshot(name) => {
                let image = match self.load(name, held).await {
                    Ok(image) => image,
                    Err(refusal) => return refusal.into_response(),
                };
                let handler = handler.filesystem(Box::new(image.fs())).build_handler();
                let response = handler.handle(request).await;
                match image.save().await {
                    Ok(()) => response.map(Body::new),
                    Err(e) => refused(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
                }
            }
        }
    }

    /// The snapshot `name` read whole, its requests before this one done and nothing else
    /// holding it: `404` for none here, `423` for one in use.
    async fn load(&self, name: &str, held: Held<'_>) -> Result<snapshot::Image, Refusal> {
        let snapshot = self
            .snapshots
            .open(name)
            .map_err(|e| refused(snapshot_status(&e), e))?;
        let serial = Arc::clone(
            self.serial
                .lock()
                .unwrap()
                .entry(name.to_owned())
                .or_default(),
        );
        let serial = serial.lock_owned().await;
        if held(name) {
            return Err(refused(
                StatusCode::LOCKED,
                SnapshotError::InUse(name.to_owned()),
            ));
        }
        snapshot::Image::load(snapshot, serial)
            .await
            .map_err(|e| refused(snapshot_status(&e), e))
    }
}

/// What answers a WebDAV request under `prefix`, but its filesystem: refused for a write of part
/// of a file, since a file is written whole, and for one that [takes the root](takes_root).
fn handler(prefix: String, request: &Request<Body>) -> Result<dav_server::DavConfig, Refusal> {
    if request.method() == Method::PUT && request.headers().contains_key(header::CONTENT_RANGE) {
        return Err(refused(StatusCode::BAD_REQUEST, "a file is written whole"));
    }
    if takes_root(&prefix, request) {
        return Err(refused(
            StatusCode::FORBIDDEN,
            "the root is removed through its resource",
        ));
    }
    Ok(DavHandler::builder()
        .strip_prefix(prefix)
        .hide_symlinks(true)
        .read_buf_size(volume::READ))
}

/// Whether `request` would take the root at `prefix` away or write over it: a `DELETE` or `MOVE`
/// of it, or a `MOVE` or `COPY` onto it. `dav-server` does either a member at a time, so a
/// filesystem refusing the root itself refuses it only once all in it has gone.
fn takes_root(prefix: &str, request: &Request<Body>) -> bool {
    let is_root = |path: &str| path.trim_end_matches('/') == prefix;
    let method = request.method().as_str();
    let destination = request
        .headers()
        .get("Destination")
        .and_then(|to| to.to_str().ok()?.parse::<http::Uri>().ok());
    (matches!(method, "DELETE" | "MOVE") && is_root(request.uri().path()))
        || (matches!(method, "MOVE" | "COPY") && destination.is_some_and(|to| is_root(to.path())))
}

/// Whether `request` is a conditional write: one that may change what is at its path, with
/// `If-Match` or `If-None-Match`. A read with either is not: `If-None-Match` on a `GET` is a
/// cache asking whether it is current.
fn conditional<B>(request: &Request<B>) -> bool {
    let method = request.method();
    let reads = matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS)
        || method.as_str() == "PROPFIND";
    let headers = request.headers();
    !reads
        && (headers.contains_key(header::IF_MATCH) || headers.contains_key(header::IF_NONE_MATCH))
}

/// `request` with its body read to its end first, into a file of its own that goes once it
/// is closed, and read back from it: what holds a root while `dav-server` answers the request
/// holds it only as long as reading a local file takes, not as long as the sender does. One
/// with no body, as a `DELETE` has, is left as it is.
async fn spooled(request: Request<Body>) -> Result<Request<Body>, Refusal> {
    use tokio::io::{AsyncSeekExt as _, AsyncWriteExt as _};

    if http_body::Body::is_end_stream(request.body()) {
        return Ok(request);
    }

    let failed = |e: io::Error| refused(StatusCode::INTERNAL_SERVER_ERROR, e);
    let (parts, mut body) = request.into_parts();
    // Written a quarter of a MiB at a time, not as each small piece arrives.
    let mut file = tokio::io::BufWriter::with_capacity(
        256 << 10,
        tokio::fs::File::from_std(tempfile::tempfile().map_err(failed)?),
    );
    while let Some(frame) = body.frame().await {
        let frame = frame
            .map_err(|e| refused(StatusCode::BAD_REQUEST, format!("the body broke off: {e}")))?;
        if let Ok(data) = frame.into_data() {
            file.write_all(&data).await.map_err(failed)?;
        }
    }
    file.flush().await.map_err(failed)?;
    let mut file = file.into_inner();
    file.rewind().await.map_err(failed)?;
    // Read back as much at a time as a volume's `GET` reads, since a volume opens the file again
    // for each piece it writes.
    let body = Body::from_stream(tokio_util::io::ReaderStream::with_capacity(
        file,
        volume::READ,
    ));
    Ok(Request::from_parts(parts, body))
}

/// The first segment of `rest`, what follows a root's kind in a path.
fn name(rest: &str) -> String {
    rest.split('/').next().unwrap_or("").to_owned()
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

/// The status a request on a volume's files is refused with when it fails with `e`, as a
/// WebDAV request would be: `404` for nothing there, `409` for a parent that is not a
/// directory, `403` for a change a mount or the volume will not take.
fn status(e: &VolumeError) -> StatusCode {
    match e {
        VolumeError::InvalidName(_) | VolumeError::InvalidPath(_) => StatusCode::BAD_REQUEST,
        VolumeError::Unknown(_) | VolumeError::NoMount { .. } => StatusCode::NOT_FOUND,
        VolumeError::AlreadyExists(_) | VolumeError::MountExists(_) => StatusCode::CONFLICT,
        VolumeError::InUse(_) => StatusCode::LOCKED,
        VolumeError::Refused(_) => StatusCode::FORBIDDEN,
        VolumeError::Io { source, .. } => match source.kind() {
            io::ErrorKind::NotFound => StatusCode::NOT_FOUND,
            io::ErrorKind::NotADirectory | io::ErrorKind::IsADirectory => StatusCode::CONFLICT,
            io::ErrorKind::AlreadyExists => StatusCode::CONFLICT,
            io::ErrorKind::PermissionDenied | io::ErrorKind::ReadOnlyFilesystem => {
                StatusCode::FORBIDDEN
            }
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        },
        VolumeError::Source(_) | VolumeError::Origins(..) => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

/// The status a request on a snapshot's files is refused with when opening or holding the
/// snapshot fails with `e`.
fn snapshot_status(e: &SnapshotError) -> StatusCode {
    match e {
        SnapshotError::InvalidName(_) => StatusCode::BAD_REQUEST,
        SnapshotError::Unknown(_) => StatusCode::NOT_FOUND,
        SnapshotError::InUse(_) => StatusCode::LOCKED,
        SnapshotError::AlreadyExists(_) | SnapshotError::Fstree(_) | SnapshotError::Io { .. } => {
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}
