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

mod sandbox;
mod snapshot;
mod volume;

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use axum::body::Body;
use axum::response::{IntoResponse, Response};
use dav_server::DavHandler;
use dav_server::davpath::DavPath;
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
    /// and each volume's that write over an etag, by `volume:` and its name, so the etag each
    /// checks is the one the file has when it writes.
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
        let handler = handler
            .filesystem(Box::new(sandbox::Fs(machine)))
            .build_handler();
        let response = lingering(request, |request| handler.handle(request)).await;
        response.map(|body| {
            Body::new(body.map_frame(move |frame| {
                let _ = &activity;
                frame
            }))
        })
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
                let handler = handler
                    .filesystem(Box::new(volume::Fs(volume)))
                    .build_handler();
                let conditional = request.headers().contains_key(http::header::IF_MATCH);
                let _serial = match conditional {
                    true => {
                        let key = format!("volume:{name}");
                        let serial =
                            Arc::clone(self.serial.lock().unwrap().entry(key).or_default());
                        Some(serial.lock_owned().await)
                    }
                    false => None,
                };
                lingering(request, |request| handler.handle(request))
                    .await
                    .map(Body::new)
            }
            Root::Snapshot(name) => {
                let image = match self.load(name, held).await {
                    Ok(image) => image,
                    Err(refusal) => return refusal.into_response(),
                };
                let handler = handler.filesystem(Box::new(image.fs())).build_handler();
                let response = lingering(request, |request| handler.handle(request)).await;
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

/// `answer` to `request`, once what `answer` left unread of its body has been read.
///
/// An answer made before the body is read -- a write refused over an etag gone stale -- would
/// otherwise go out while the body is still arriving, and the connection, with it unread, is
/// closed: Windows resets it, and a client still sending sees the reset and never the answer.
/// Read to its end, the body leaves the connection as one whose request was whole.
async fn lingering<B, F>(
    request: Request<Body>,
    answer: impl FnOnce(Request<Shared>) -> F,
) -> http::Response<B>
where
    F: Future<Output = http::Response<B>>,
{
    let (parts, body) = request.into_parts();
    let body = Shared(Arc::new(Mutex::new(body)));
    let response = answer(Request::from_parts(parts, body.clone())).await;
    let mut rest = body;
    while let Some(Ok(_)) = rest.frame().await {}
    response
}

/// A request's body, which [`lingering`] reads the rest of after the handler it was given to.
#[derive(Clone)]
struct Shared(Arc<Mutex<Body>>);

impl http_body::Body for Shared {
    type Data = bytes::Bytes;
    type Error = axum::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        Pin::new(&mut *self.0.lock().unwrap()).poll_frame(cx)
    }
}

/// What answers a WebDAV request under `prefix`, but its filesystem: refused for a write of part
/// of a file, since a file is written whole, and for one that [takes the root](takes_root).
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
    Ok(DavHandler::builder()
        .strip_prefix(prefix)
        .hide_symlinks(true)
        .read_buf_size(volume::READ))
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

#[cfg(test)]
mod tests {
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
}
