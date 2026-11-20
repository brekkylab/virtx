//! The files endpoints of `virtx-core`'s `PROTOCOL.md`: the files of a volume or a sandbox over
//! WebDAV at `/volumes/{name}/files/` and `/sandboxes/{id}/files/`.
//!
//! A volume's files are the directory on this host a machine shares, its mounts at their
//! paths. A sandbox's are its machine's root as its processes see it, asked of the agent there
//! ([`Machine::io`]).
//!
//! A `PUT` to a volume or a sandbox lands whole: its body is written beside its path and moved
//! over it once all of it has arrived, as `dav-server` flushes the file. One with `If-Match` or
//! `If-None-Match` is checked by `dav-server` before its body is read, and [again as it
//! lands](Landing), holding the volume or the sandbox: against what is at the path now, and the
//! write moved over it in the same hold, or let go and refused `412`. Of two that checked one
//! state, the second is checked against what the first wrote, and a slow or stalled sender holds
//! up no other writer. Any other write with either condition is held while `dav-server` answers
//! it. A sandbox's write holds each volume the sandbox mounts writable too, after the sandbox
//! and in the order of their directories, so one through a mount and one through the volume's
//! own files are held against each other. A `PUT` to a path in such a mount is checked against
//! the volume's own files on this host as it lands, not as the guest sees them: the guest may
//! hold what it saw there for a few seconds after a change made outside it. The rest is checked
//! as the guest sees it: a `PUT` before its body is read, the condition of any other write, and
//! a path the volume's files do not answer for, through a link, by a name they refuse, or in a
//! mount of the volume's own.
//!
//! Where a volume is kept beyond this machine ([`Files::backed_by`]), it is brought up to what is
//! kept before a request is answered from it, and held there while a request writes it.

mod sandbox;
mod volume;

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use axum::body::Body;
use axum::response::{IntoResponse, Response};
use dav_server::DavHandler;
use dav_server::davpath::DavPath;
use dav_server::fs::{DavMetaData, FsError};
use futures_util::future::BoxFuture;
use http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt as _;

use crate::HostPaths;
use crate::backend::{self, Backing, Stored};
use crate::machine::Machine;
use crate::sandbox::Activity;
use crate::volume::{Error as VolumeError, Store as VolumeStore};

/// The files endpoints over one home's volumes, and its sandboxes' as each is asked of.
pub struct Files {
    volumes: VolumeStore,
    /// Each volume's [conditional writes](conditional), by `volume:` and its name, before
    /// its [lock file](writing) too, through its own files or a sandbox that mounts it. A
    /// sandbox's are held by the sandbox's own first.
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
        let handler = match handler(format!("/volumes/{name}/files"), &request) {
            Ok(handler) => handler,
            Err(refusal) => return refusal.into_response(),
        };
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
        if request.method() == Method::PATCH {
            let fs = volume::Fs {
                volume,
                landing: None,
            };
            return not_allowed(handler.filesystem(Box::new(fs)).build_handler(), request).await;
        }
        let hold = || -> Hold {
            let serial = self.serial(format!("volume:{name}"));
            let dir = volume.path().to_owned();
            Arc::new(move || {
                let (serial, dir) = (Arc::clone(&serial), dir.clone());
                Box::pin(async move { Ok(Box::new(held_volume(serial, &dir).await?) as Guard) })
            })
        };
        let (request, landing, _held) = match conditioned(request, hold).await {
            Ok(conditioned) => conditioned,
            Err(refusal) => return refusal.into_response(),
        };
        let fs = volume::Fs {
            volume,
            landing: landing.clone(),
        };
        let handler = handler.filesystem(Box::new(fs)).build_handler();
        let response = lingering(request, |request| handler.handle(request)).await;
        landed(landing.as_deref(), response.map(Body::new))
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
        let sandbox = activity.sandbox();
        let mounts: Arc<[(String, PathBuf)]> = sandbox.mounts().into();
        if request.method() == Method::PATCH {
            let fs = sandbox::Fs {
                machine,
                landing: None,
                mounts,
            };
            return not_allowed(handler.filesystem(Box::new(fs)).build_handler(), request).await;
        }
        let hold = || -> Hold {
            let writing = Arc::clone(sandbox.writing());
            let mut dirs: Vec<PathBuf> = mounts.iter().map(|(_, dir)| dir.clone()).collect();
            dirs.sort();
            dirs.dedup();
            let volumes: Vec<_> = dirs
                .into_iter()
                .map(|dir| {
                    let name = dir.file_name().unwrap_or_default().to_string_lossy();
                    (self.serial(format!("volume:{name}")), dir)
                })
                .collect();
            Arc::new(move || {
                let (writing, volumes) = (Arc::clone(&writing), volumes.clone());
                Box::pin(async move {
                    let sandbox = writing.lock_owned().await;
                    let mut held = Vec::with_capacity(volumes.len());
                    for (serial, dir) in volumes {
                        held.push(held_volume(serial, &dir).await?);
                    }
                    Ok(Box::new((sandbox, held)) as Guard)
                })
            })
        };
        let (request, landing, _held) = match conditioned(request, hold).await {
            Ok(conditioned) => conditioned,
            Err(refusal) => return refusal.into_response(),
        };
        let fs = sandbox::Fs {
            machine,
            landing: landing.clone(),
            mounts: Arc::clone(&mounts),
        };
        let handler = handler.filesystem(Box::new(fs)).build_handler();
        let response = lingering(request, |request| handler.handle(request)).await;
        landed(landing.as_deref(), response.map(Body::new)).map(|body| {
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
}

/// `answer` to `request`, once what `answer` left unread of its body has been read.
///
/// An answer made before the body is read -- a `PUT` refused under a directory that is not
/// there -- would otherwise go out while the body is still arriving, and the connection, with it
/// unread, is closed: Windows resets it, and a client still sending sees the reset and never the
/// answer. Read to its end, the body leaves the connection as one whose request was whole.
async fn lingering<B, F>(
    request: Request<Body>,
    answer: impl FnOnce(Request<Shared>) -> F,
) -> http::Response<B>
where
    F: Future<Output = http::Response<B>>,
{
    let (parts, body) = request.into_parts();
    let body = Shared(Arc::new(Mutex::new(Some(body))));
    let response = answer(Request::from_parts(parts, body.clone())).await;
    let mut rest = body;
    while let Some(Ok(_)) = rest.frame().await {}
    response
}

/// A request's body, which [`lingering`] reads the rest of after the handler it was given to:
/// once it has ended it is not read again, so a handler that read it to its end leaves
/// [`lingering`] nothing, whatever the body is.
#[derive(Clone)]
struct Shared(Arc<Mutex<Option<Body>>>);

impl http_body::Body for Shared {
    type Data = bytes::Bytes;
    type Error = axum::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        let mut body = self.0.lock().unwrap();
        let Some(rest) = body.as_mut() else {
            return Poll::Ready(None);
        };
        let polled = Pin::new(rest).poll_frame(cx);
        if let Poll::Ready(None) = polled {
            *body = None;
        }
        polled
    }
}

/// What answers a WebDAV request under `prefix`, but its filesystem: refused for a `PUT` of part
/// of a file, with `Content-Range`, since a file is written whole, and for one that
/// [takes the root](takes_root). A `PATCH` is [not allowed](not_allowed).
fn handler(prefix: String, request: &Request<Body>) -> Result<dav_server::DavConfig, Refusal> {
    if request.method() == Method::PUT && request.headers().contains_key(header::CONTENT_RANGE) {
        return Err(refused(StatusCode::BAD_REQUEST, "a file is written whole"));
    }
    if takes_root(&prefix, request)? {
        return Err(refused(
            StatusCode::FORBIDDEN,
            "the root is removed through its resource",
        ));
    }
    // `Allow`, which `OPTIONS` answers and `not_allowed` sends with its `405`, leaves `PATCH`
    // out. Given a set of methods with `PROPFIND` in it, `dav-server` would answer a `GET` of a
    // directory with an HTML index of it; a directory is listed with `PROPFIND`.
    let mut methods = dav_server::DavMethodSet::all();
    methods.remove(dav_server::DavMethod::Patch);
    Ok(DavHandler::builder()
        .methods(methods)
        .autoindex(false)
        .strip_prefix(prefix)
        .hide_symlinks(true)
        .read_buf_size(volume::READ))
}

/// `request`, a `PATCH`, refused `405 method_not_allowed` once its body is read, with the `Allow`
/// that `handler` answers an `OPTIONS` of its path with: a file is written whole, by `PUT`.
async fn not_allowed(handler: DavHandler, request: Request<Body>) -> Response {
    let (parts, mut body) = request.into_parts();
    while let Some(Ok(_)) = body.frame().await {}
    let mut options = Request::new(Body::empty());
    *options.method_mut() = Method::OPTIONS;
    *options.uri_mut() = parts.uri;
    let allow = handler
        .handle(options)
        .await
        .headers()
        .get(header::ALLOW)
        .cloned();
    let error = serde_json::json!({"error": {
        "code": "method_not_allowed",
        "message": "a file is written whole, by PUT",
    }});
    let json = [(header::CONTENT_TYPE, "application/json")];
    let mut response = (StatusCode::METHOD_NOT_ALLOWED, json, error.to_string()).into_response();
    if let Some(allow) = allow {
        response.headers_mut().insert(header::ALLOW, allow);
    }
    response
}

/// Whether `request` would take the root at `prefix` away or write over it: a `DELETE` or `MOVE`
/// of it, or a `MOVE` or `COPY` onto it. `dav-server` does either a member at a time, so a
/// filesystem refusing the root itself refuses it only once all in it has gone.
///
/// A path or a `Destination` with a `.` or `..` segment, plain or percent-encoded, is refused
/// with `400`: a client resolves them before it sends a path, as RFC 3986 has it, and the root
/// has one spelling. A `\` separates segments here too, as it does in an `http` URL. Past that,
/// each is read as `dav-server` reads it, and one that is not under `prefix` so read is refused
/// with `400` too: what a URL drops before it resolves, such as a tab, is caught there.
fn takes_root(prefix: &str, request: &Request<Body>) -> Result<bool, Refusal> {
    let method = request.method().as_str();
    // A `Destination` up to its query or fragment, which `dav-server` drops.
    let to = match matches!(method, "MOVE" | "COPY") {
        true => request
            .headers()
            .get("Destination")
            .and_then(|to| to.to_str().ok()?.split(['?', '#']).next()),
        false => None,
    };
    if let Some(dotted) = [Some(request.uri().path()), to]
        .into_iter()
        .flatten()
        .find(|path| path.split(['/', '\\']).any(is_dot))
    {
        return Err(refused(
            StatusCode::BAD_REQUEST,
            format!("{dotted:?} has a \".\" or \"..\" segment, which a client resolves"),
        ));
    }
    let at = place(prefix, request.uri().path())
        .ok_or_else(|| refused(StatusCode::BAD_REQUEST, format!("not a path in {prefix}")))?;
    let onto = match matches!(method, "MOVE" | "COPY") {
        true => match destination(request) {
            Some(to) => Some(place(prefix, &to).ok_or_else(|| {
                refused(
                    StatusCode::BAD_REQUEST,
                    format!("the destination is not a path in {prefix}"),
                )
            })?),
            None => None,
        },
        false => None,
    };
    Ok((matches!(method, "DELETE" | "MOVE") && at == Place::Root) || onto == Some(Place::Root))
}

/// Whether `segment`, percent-encoded, is `.` or `..`.
fn is_dot(segment: &str) -> bool {
    matches!(
        segment.to_ascii_lowercase().replace("%2e", ".").as_str(),
        "." | ".."
    )
}

/// Where a path is in the root at `prefix`.
#[derive(Debug, PartialEq, Eq)]
enum Place {
    Root,
    Under,
}

/// Where `path`, percent-encoded, is in the root at `prefix` once `dav-server` has decoded it and
/// resolved its `.` and `..`; `None` for one it cannot read, or one not in the root. A path the
/// prefix only begins, such as `{prefix}x`, is not in it, though `dav-server` takes it as one.
fn place(prefix: &str, path: &str) -> Option<Place> {
    let path = DavPath::new(path).ok()?;
    match path
        .with_prefix()
        .as_bytes()
        .strip_prefix(prefix.as_bytes())?
    {
        b"" | b"/" => Some(Place::Root),
        rest if rest.starts_with(b"/") => Some(Place::Under),
        _ => None,
    }
}

/// The path of `request`'s `Destination`, as `dav-server` takes it: the header as it is when it
/// is a path, or the path of the URL it is. `None` for none, or one `dav-server` cannot read,
/// which it refuses itself.
fn destination(request: &Request<Body>) -> Option<String> {
    let to = request.headers().get("Destination")?.to_str().ok()?;
    match to.starts_with('/') {
        true => Some(to.to_owned()),
        false => Some(to.parse::<url::Url>().ok()?.path().to_owned()),
    }
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

/// What holds a volume or a sandbox for a conditional write, let go of when it drops.
type Guard = Box<dyn Send>;

/// What takes a [`Guard`], waiting while another holds it, or why it could not.
type Hold = Arc<dyn Fn() -> BoxFuture<'static, Result<Guard, Refusal>> + Send + Sync>;

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

/// The volume whose own directory is `dir` held, under `serial`, for one conditional write, as
/// [`writing`] holds it; or why not: `404` for a volume that is not there any more.
async fn held_volume(
    serial: Arc<tokio::sync::Mutex<()>>,
    dir: &Path,
) -> Result<(std::fs::File, tokio::sync::OwnedMutexGuard<()>), Refusal> {
    let name = dir.file_name().unwrap_or_default().to_string_lossy();
    writing(serial, dir).await.map_err(|e| {
        if e.kind() == io::ErrorKind::NotFound {
            let gone = VolumeError::Unknown(name.into_owned());
            return refused(status(&gone), gone);
        }
        let why = format!("holding {name} for a write: {e}");
        refused(StatusCode::INTERNAL_SERVER_ERROR, why)
    })
}

/// `request` made ready to answer under the [`Hold`] `hold` makes: a conditional `PUT` with the
/// [`Landing`] its write is checked by as it lands, holding nothing before; any other
/// conditional write [gathered], and held while it is answered; anything else as it is, with no
/// hold made for it.
async fn conditioned(
    request: Request<Body>,
    hold: impl FnOnce() -> Hold,
) -> Result<(Request<Body>, Option<Arc<Landing>>, Option<Guard>), Refusal> {
    if !conditional(&request) {
        return Ok((request, None, None));
    }
    let hold = hold();
    if request.method() == Method::PUT {
        let landing = Landing {
            condition: Condition::of(request.headers()),
            hold,
            refusal: Mutex::default(),
        };
        return Ok((request, Some(Arc::new(landing)), None));
    }
    let request = gathered(request).await?;
    let held = hold().await?;
    Ok((request, None, Some(held)))
}

/// A conditional `PUT`'s condition, checked again as its write lands: `dav-server` checks it
/// before it reads the body, and two that checked one state would otherwise both land.
pub(super) struct Landing {
    condition: Condition,
    hold: Hold,
    /// What the request is answered with instead, once its write was let go here: `412`, or
    /// why its root could not be held.
    refusal: Mutex<Option<Refusal>>,
}

impl Landing {
    /// Holding the root, check the condition against what `there` finds at the path, and `land`
    /// the write if it holds; or let it go, failing, and answer the request [as it
    /// says](landed). The hold is waited for here, so a write whose client is gone stops
    /// waiting; once it is held, the check and the move go on in a task of its own, which keeps
    /// the hold until it has done either: a client gone then, dropping the request, would
    /// otherwise let go of the hold while a move to a sandbox's guest is still on its way.
    pub(super) async fn land<M: DavMetaData + 'static>(
        self: &Arc<Self>,
        there: impl Future<Output = Result<Option<M>, FsError>> + Send + 'static,
        land: impl Future<Output = Result<(), FsError>> + Send + 'static,
    ) -> Result<(), FsError> {
        let held = match (self.hold)().await {
            Ok(held) => held,
            Err(refusal) => return Err(self.refuse(refusal)),
        };
        let landing = Arc::clone(self);
        let task = tokio::spawn(async move {
            let _held = held;
            let there = there.await?;
            if !landing
                .condition
                .holds(there.as_ref().map(|meta| meta as &dyn DavMetaData))
            {
                return Err(landing.refuse(stale()));
            }
            land.await
        });
        task.await.unwrap_or(Err(FsError::GeneralFailure))
    }

    /// Answer `refusal`, failing the write.
    fn refuse(&self, refusal: Refusal) -> FsError {
        *self.refusal.lock().unwrap() = Some(refusal);
        FsError::GeneralFailure
    }
}

/// `response`, or what its write was refused with as it [landed](Landing::land), if it was.
fn landed(landing: Option<&Landing>, response: Response) -> Response {
    match landing.and_then(|landing| landing.refusal.lock().unwrap().take()) {
        Some(refusal) => refusal.into_response(),
        None => response,
    }
}

/// A conditional write refused over a file not the one its condition named.
fn stale() -> Refusal {
    refused(
        StatusCode::PRECONDITION_FAILED,
        "the file is not the one its condition named",
    )
}

/// The most of the body of a conditional request other than a `PUT` that is read before its
/// root is held: what `dav-server` itself reads of one.
const GATHERED: usize = 64 << 10;

/// `request`, a conditional write other than a `PUT`, with its body read to its end first, up to
/// [`GATHERED`], so a sender that stalls holds up no other writer while its root is held.
async fn gathered(request: Request<Body>) -> Result<Request<Body>, Refusal> {
    let (parts, body) = request.into_parts();
    match http_body_util::Limited::new(body, GATHERED).collect().await {
        Ok(body) => Ok(Request::from_parts(parts, Body::from(body.to_bytes()))),
        Err(e) if e.is::<http_body_util::LengthLimitError>() => {
            Err(refused(StatusCode::PAYLOAD_TOO_LARGE, e))
        }
        Err(e) => Err(refused(
            StatusCode::BAD_REQUEST,
            format!("the body broke off: {e}"),
        )),
    }
}

/// A request's `If-Match` and `If-None-Match`: each a list of tags or `*`. `If-Match` compares
/// tags strongly, a weak one naming nothing, and `If-None-Match` weakly, `W/` or not, as RFC 9110
/// section 13.1 has it. A line that is not text names nothing, where `dav-server` takes the
/// header as absent: an `If-Match` of only such lines is let through before the body and refused
/// as it lands.
struct Condition {
    matching: Option<Tags>,
    none_matching: Option<Tags>,
}

enum Tags {
    Any,
    These(Vec<String>),
}

impl Condition {
    fn of(headers: &http::HeaderMap) -> Condition {
        Condition {
            matching: Tags::of(headers, header::IF_MATCH),
            none_matching: Tags::of(headers, header::IF_NONE_MATCH),
        }
    }

    /// Whether it holds over `there`, what is at the path, or nothing.
    fn holds(&self, there: Option<&dyn DavMetaData>) -> bool {
        self.matching
            .as_ref()
            .is_none_or(|tags| tags.name(there, false))
            && !self
                .none_matching
                .as_ref()
                .is_some_and(|tags| tags.name(there, true))
    }
}

impl Tags {
    fn of(headers: &http::HeaderMap, name: header::HeaderName) -> Option<Tags> {
        let mut values = headers.get_all(name).iter().peekable();
        values.peek()?;
        let mut these = Vec::new();
        for value in values {
            let Ok(value) = value.to_str() else { continue };
            if value.trim() == "*" {
                return Some(Tags::Any);
            }
            these.extend(value.split(',').map(str::trim).map(str::to_owned));
        }
        Some(Tags::These(these))
    }

    /// Whether they name `there`: `*` anything, a tag the one it has, and, compared `weak`ly,
    /// that one marked weak too. Every tag here is strong.
    fn name(&self, there: Option<&dyn DavMetaData>, weak: bool) -> bool {
        match self {
            Tags::Any => there.is_some(),
            Tags::These(these) => there.and_then(|meta| meta.etag()).is_some_and(|etag| {
                let tag = format!("\"{etag}\"");
                these.iter().any(|given| {
                    *given == tag || (weak && given.strip_prefix("W/") == Some(tag.as_str()))
                })
            }),
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

#[cfg(test)]
mod tests {
    use futures_util::StreamExt as _;

    use super::*;

    const PREFIX: &str = "/volumes/v/files";

    /// `takes_root` of a `method` request to `path`, with `destination` if given: whether it
    /// does, or the status it is refused with.
    fn takes(method: &str, path: &str, destination: Option<&str>) -> Result<bool, StatusCode> {
        let mut request = Request::builder().method(method).uri(path);
        if let Some(to) = destination {
            request = request.header("Destination", to);
        }
        takes_root(PREFIX, &request.body(Body::empty()).unwrap()).map_err(|refusal| refusal.0)
    }

    #[test]
    fn the_root_is_taken_by_a_delete_or_move_of_it_or_onto_it() {
        for root in [
            "/volumes/v/files",
            "/volumes/v/files/",
            "/volumes/v/files//",
        ] {
            assert_eq!(takes("DELETE", root, None), Ok(true), "DELETE {root}");
            assert_eq!(
                takes("MOVE", root, Some("/volumes/v/files/b")),
                Ok(true),
                "MOVE {root}"
            );
            assert_eq!(takes("GET", root, None), Ok(false), "GET {root}");
            assert_eq!(
                takes("COPY", root, Some("/volumes/v/files/b")),
                Ok(false),
                "COPY {root}"
            );
            assert_eq!(
                takes("COPY", "/volumes/v/files/a", Some(root)),
                Ok(true),
                "COPY onto {root}"
            );
            let url = format!("http://virtx{root}");
            assert_eq!(
                takes("MOVE", "/volumes/v/files/a", Some(&url)),
                Ok(true),
                "MOVE onto {url}"
            );
        }
    }

    #[test]
    fn what_is_in_the_root_is_not_the_root() {
        for path in [
            "/volumes/v/files/a",
            "/volumes/v/files/a/b/",
            "/volumes/v/files/a.b/..c/.d",
        ] {
            assert_eq!(takes("DELETE", path, None), Ok(false), "DELETE {path}");
            assert_eq!(
                takes("MOVE", "/volumes/v/files/c", Some(path)),
                Ok(false),
                "MOVE onto {path}"
            );
        }
    }

    #[test]
    fn a_dot_segment_is_a_bad_request() {
        for dotted in [
            "/volumes/v/files/.",
            "/volumes/v/files/./",
            "/volumes/v/files/%2e",
            "/volumes/v/files/%2E/",
            "/volumes/v/files/a/..",
            "/volumes/v/files/a/%2e%2E/",
            "/volumes/v/files/..",
            "/volumes/v/files/../../w/files/a",
        ] {
            for method in ["GET", "DELETE", "MOVE", "COPY"] {
                assert_eq!(
                    takes(method, dotted, Some("/volumes/v/files/b")),
                    Err(StatusCode::BAD_REQUEST),
                    "{method} {dotted}"
                );
            }
            let url = format!("http://virtx{dotted}");
            for to in [dotted, url.as_str()] {
                assert_eq!(
                    takes("MOVE", "/volumes/v/files/a", Some(to)),
                    Err(StatusCode::BAD_REQUEST),
                    "MOVE onto {to}"
                );
            }
        }
        // A `Destination` is read up to its query, as `dav-server` reads it.
        assert_eq!(
            takes(
                "MOVE",
                "/volumes/v/files/c",
                Some("/volumes/v/files/a/b/..?")
            ),
            Err(StatusCode::BAD_REQUEST)
        );
        assert_eq!(
            takes(
                "MOVE",
                "/volumes/v/files/c",
                Some("/volumes/v/files/a?q=/..")
            ),
            Ok(false)
        );
        // Only a `MOVE` or a `COPY` reads its `Destination`.
        assert_eq!(
            takes("DELETE", "/volumes/v/files/a", Some("/volumes/v/files/.")),
            Ok(false)
        );
    }

    #[test]
    fn what_a_url_reads_as_a_dot_segment_is_refused_before_anything_goes() {
        let from = "/volumes/v/files/a";
        // An `http` URL takes `\` for `/`: a dot segment like any other.
        for to in [
            "http://virtx/volumes/v/files/a\\..",
            "http://virtx/volumes/v/files\\.",
        ] {
            assert_eq!(
                takes("MOVE", from, Some(to)),
                Err(StatusCode::BAD_REQUEST),
                "MOVE onto {to:?}"
            );
        }
        // It drops a tab before it resolves: what that leaves is read as `dav-server` reads it.
        assert_eq!(
            takes("MOVE", from, Some("http://virtx/volumes/v/files/.\t.")),
            Err(StatusCode::BAD_REQUEST)
        );
        assert_eq!(
            takes("MOVE", from, Some("http://virtx/volumes/v/files/a/.\t.")),
            Ok(true)
        );
        // What only looks like a dot is a name: encoded twice, or a full-width one.
        for name in ["%252e", "%252e%252e", "%EF%BC%8E", "%EF%BC%8E%EF%BC%8E"] {
            let path = format!("/volumes/v/files/{name}");
            assert_eq!(takes("DELETE", &path, None), Ok(false), "DELETE {path}");
        }
    }

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
    async fn a_write_of_part_of_a_file_is_refused_and_leaves_it() {
        let scratch = tempfile::tempdir().unwrap();
        let volumes = VolumeStore::new(scratch.path().join("volume")).unwrap();
        volumes.create("v").unwrap();
        let files = Files::new(volumes);
        let path = "/volumes/v/files/a.txt";
        let put = Request::put(path).body(Body::from("0123456789")).unwrap();
        assert_eq!(files.volume("v", put).await.status(), StatusCode::CREATED);
        let etag = {
            let got = files.volume("v", Request::get(path).body(Body::empty()).unwrap());
            got.await.headers()[header::ETAG].clone()
        };

        let options = Request::options(path).body(Body::empty()).unwrap();
        let allow = files.volume("v", options).await.headers()[header::ALLOW].clone();
        let allow = allow.to_str().unwrap();
        assert!(allow.contains("PUT"), "{allow}");
        assert!(!allow.contains("PATCH"), "OPTIONS offers a PATCH: {allow}");
        let root = Request::get("/volumes/v/files/").body(Body::empty());
        let status = files.volume("v", root.unwrap()).await.status();
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "GET of a directory");

        let partial = [
            ("a PATCH", Method::PATCH, None),
            ("a PATCH with If-Match", Method::PATCH, Some(etag)),
        ];
        for (what, method, if_match) in partial {
            let mut request = Request::builder()
                .method(method)
                .uri(path)
                .header(header::CONTENT_TYPE, "application/x-sabredav-partialupdate")
                .header("X-Update-Range", "bytes=2-17")
                .header(header::CONTENT_LENGTH, 16);
            if let Some(etag) = if_match {
                request = request.header(header::IF_MATCH, etag);
            }
            // Its body in 16 parts, each counted as it is read.
            let read = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let counted = Arc::clone(&read);
            let parts = futures_util::stream::iter(0..16).map(move |_| {
                counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok::<_, io::Error>(bytes::Bytes::from("Y"))
            });
            let body = Body::from_stream(parts);
            let response = files.volume("v", request.body(body).unwrap()).await;
            let read = read.load(std::sync::atomic::Ordering::SeqCst);
            assert_eq!(read, 16, "{what}: its body read to its end");
            assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED, "{what}");
            assert_eq!(response.headers()[header::ALLOW], allow, "{what}");
            let body = response.into_body().collect().await.unwrap().to_bytes();
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(body["error"]["code"], "method_not_allowed", "{what}");
        }
        let ranged = Request::put(path)
            .header(header::CONTENT_RANGE, "bytes 0-0/10")
            .body(Body::from("Y"))
            .unwrap();
        assert_eq!(
            files.volume("v", ranged).await.status(),
            StatusCode::BAD_REQUEST,
            "a PUT with Content-Range"
        );

        let got = files.volume("v", Request::get(path).body(Body::empty()).unwrap());
        let body = got.await.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(body, "0123456789");
    }

    #[test]
    fn a_path_out_of_the_root_is_a_bad_request() {
        for out in ["/volumes/v/filesx", "/volumes/w/files/a", "/volumes/v"] {
            assert_eq!(
                takes("GET", out, None),
                Err(StatusCode::BAD_REQUEST),
                "GET {out}"
            );
            assert_eq!(
                takes("MOVE", "/volumes/v/files/a", Some(out)),
                Err(StatusCode::BAD_REQUEST),
                "MOVE onto {out}"
            );
        }
    }

    #[tokio::test]
    async fn a_volume_that_does_not_open_is_refused_by_the_kind_of_its_failure() {
        let scratch = tempfile::tempdir().unwrap();
        let volumes = VolumeStore::new(scratch.path()).unwrap();
        volumes.create("v").unwrap();
        let table = scratch.path().join("volume/v/mounts.json");
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
