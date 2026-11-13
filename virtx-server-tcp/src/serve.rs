//! The server's routes and loops, for a [`crate::access::Access`]: the endpoints of
//! `virtx-core`'s `PROTOCOL.md` ([`crate::endpoints`]), each answered from the home its request
//! reaches, the images as an OCI registry, and each sandbox's ports; and sandboxes stopped once
//! unused and deleted once kept past their days, images pruned. A server built on this one --
//! virtx cloud's node -- gives routes of its own to [`router`] beside them.
//!
//! The images every project shares are at `/images/oci/v2/`, as an OCI registry (see
//! [`virtx_uvm::RegistryServer`]), to any user; a tenant's project has images of its own, at
//! the same path ([`crate::access::Tenants`]).
//!
//! A sandbox's ports are at `/ports/<capability>` beside the endpoints, each a WebSocket of the
//! subprotocol `virtx.tcp` carrying a TCP connection to the port: the capability, which only
//! making the sandbox answers, is all a connection needs, and gives no more than the one port.
//!
//! A shell in a sandbox, on a terminal, is at `/terminal/{sandbox}`: a WebSocket of the
//! subprotocol `virtx.terminal` ([`terminal`]), for the same callers as the endpoints, kept
//! across connections ([`crate::terminals`]).
//!
//! A server draining ([`Access::drain`]) answers `503` to every request but its own routes', and a
//! sandbox's stop or delete.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use axum::extract::Path;
use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::middleware::Next;
use axum::response::{IntoResponse as _, Response};
use axum::routing::any;
use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tower_http::cors::CorsLayer;

use crate::access::{self, Access};
use crate::endpoints::{self, Tunnel};
use crate::terminals::{Session, Shown};

/// What this binary is spawned with to be a machine.
pub const MACHINE_SUBCOMMAND: &str = "vm";

/// The subprotocol `/terminal/{sandbox}` is spoken in.
const TERMINAL_SUBPROTOCOL: &str = "virtx.terminal";

/// The shell a terminal runs: bash where the image has it, sh otherwise, as a login shell.
/// Bash writes each command to its history as it is run, not as it exits -- a sandbox
/// stopped ends its shells without their exiting -- so that its history is on its disk, and
/// there as it starts again.
const SHELL: &str = "export TERM=xterm-256color; cd ~ 2>/dev/null; \
    if command -v bash >/dev/null 2>&1; then \
    export BASHOPTS=histappend PROMPT_COMMAND='history -a' HISTSIZE=10000 HISTFILESIZE=10000; \
    exec bash -l; else exec sh -l; fi";

/// The WebSocket subprotocol a port's TCP is carried in.
const TCP_SUBPROTOCOL: &str = "virtx.tcp";

/// How much of a port's TCP one message carries at most.
const CHUNK: usize = 64 << 10;

/// How often the images every project shares are pruned, as `POST /images/prune` does.
const PRUNE_EVERY: Duration = Duration::from_secs(60 * 60);

/// The routes, for `access`, a browser at one of `origins` let in, with `own` beside them:
/// a server built on this one's, which take no home and are not refused while it drains.
pub fn router(access: Arc<Access>, origins: &[String], own: axum::Router) -> axum::Router {
    let checked = Arc::clone(&access);
    let reaching = Arc::clone(&access);
    let reading = Arc::clone(&access);
    let opening = Arc::clone(&access);

    let images = move |request: axum::extract::Request| {
        let access = Arc::clone(&reading);
        async move {
            if access.draining() {
                return draining();
            }
            match access.registry(request.headers()).await {
                Ok((registry, home)) => {
                    // A tag named to be pulled is brought up to the storage home's index first:
                    // its image fetched, if it is not here yet. A `HEAD` of it fetches nothing:
                    // the storage home's manifest answers. A listing lists the tags still to
                    // be fetched too (the registry's `listing_too`).
                    if let Some(home) = &home
                        && let Some(reference) = tag_named(request.uri().path())
                    {
                        if request.method() == http::Method::HEAD {
                            match home.wanted_manifest(&reference).await {
                                Ok(Some((digest, manifest))) => return head(&digest, &manifest),
                                Ok(None) => {}
                                Err(e) => eprintln!("{e:#}"),
                            }
                        }
                        if let Err(e) = home.want(&reference).await {
                            eprintln!("{e:#}");
                        }
                    }
                    let by_digest = home
                        .as_ref()
                        .filter(|_| {
                            matches!(*request.method(), http::Method::GET | http::Method::HEAD)
                        })
                        .and_then(|home| {
                            digest_named(request.uri().path()).map(|named| (home.clone(), named))
                        });
                    let with_body = request.method() == http::Method::GET;
                    let answered = registry
                        .handle(request)
                        .await
                        .map(axum::body::Body::new)
                        .into_response();
                    // Not here yet, a manifest or a config named by its digest: as it is kept,
                    // its image not fetched for it.
                    if answered.status() == StatusCode::NOT_FOUND
                        && let Some((home, (part, digest))) = by_digest
                    {
                        match kept(&home, part, &digest, with_body).await {
                            Ok(Some(kept)) => return kept,
                            Ok(None) => {}
                            Err(e) => eprintln!("{e:#}"),
                        }
                    }
                    answered
                }
                Err(status) => refusal(status),
            }
        }
    };
    endpoints::router()
        .layer(axum::middleware::from_fn(
            move |mut request: axum::extract::Request, next: Next| {
                let access = Arc::clone(&checked);
                async move {
                    // Let go by the gateway: what it sends here now goes elsewhere once this
                    // node's projects are kept -- but a sandbox still running here is stopped
                    // or deleted here, which lets the node go the sooner.
                    if access.draining() && !lets_go(request.method(), request.uri().path()) {
                        return draining();
                    }
                    match access.home(request.headers()).await {
                        Ok((_, home)) => {
                            request.extensions_mut().insert(home);
                            next.run(request).await
                        }
                        Err(status) => refusal(status),
                    }
                }
            },
        ))
        .route("/images/oci/v2", any(images.clone()))
        .route("/images/oci/v2/", any(images.clone()))
        .route("/images/oci/v2/{*rest}", any(images))
        .route(
            "/terminal/{sandbox}",
            any(
                move |Path(sandbox): Path<String>,
                      uri: http::Uri,
                      headers: HeaderMap,
                      upgrade: Result<WebSocketUpgrade, WebSocketUpgradeRejection>| {
                    let access = Arc::clone(&opening);
                    let size = Size::asked(uri.query().unwrap_or_default());
                    async move { terminal(&access, sandbox, size, &headers, upgrade).await }
                },
            ),
        )
        .route(
            "/ports/{capability}",
            any(
                move |Path(capability): Path<String>,
                      headers: HeaderMap,
                      upgrade: Result<WebSocketUpgrade, WebSocketUpgradeRejection>| {
                    let access = Arc::clone(&reaching);
                    async move { port(&access, &capability, &headers, upgrade) }
                },
            ),
        )
        .merge(own)
        .layer(cors(origins))
}

/// The loops every server runs: each home's sandboxes tended -- stopped once unused, deleted once
/// kept stopped past their days -- and the images pruned.
pub fn spawn_loops(access: &Arc<Access>) {
    let reaping = Arc::clone(access);
    tokio::spawn(async move {
        // Once a minute, or as often as a minute made shorter lasts.
        let every = endpoints::minute().min(Duration::from_secs(60));
        let mut every = tokio::time::interval(every);
        loop {
            every.tick().await;
            for home in &reaping.homes() {
                home.tend();
            }
        }
    });

    let pruning = Arc::clone(access);
    tokio::spawn(async move {
        let mut every = tokio::time::interval(PRUNE_EVERY);
        loop {
            every.tick().await;
            // The shared images, and each project's own: a store pruned once however many
            // homes share it.
            let mut stores = vec![pruning.images().clone()];
            for home in pruning.homes() {
                if !stores.iter().any(|s| s.root() == home.images().root()) {
                    stores.push(home.images().clone());
                }
            }
            for images in stores {
                match tokio::task::spawn_blocking(move || images.prune(endpoints::PRUNE_GRACE))
                    .await
                {
                    Ok(Ok(_)) => {}
                    Ok(Err(e)) => eprintln!("pruning the images: {e}"),
                    Err(e) => eprintln!("pruning the images: {e}"),
                }
            }
        }
    });
}

/// Serve `router` at `listen` until interrupted.
pub async fn run(listen: std::net::SocketAddr, router: axum::Router) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("listening at {listen}"))?;
    eprintln!("serving at http://{}/", listener.local_addr()?);
    // Each write sent at once: a job's log ends with two small ones, and Nagle's algorithm
    // would hold the second for the first's delayed ACK, some 40 ms.
    let listener = axum::serve::ListenerExt::tap_io(listener, |tcp| {
        let _ = tcp.set_nodelay(true);
    });
    axum::serve(listener, router)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

/// Before any runtime: if this process was spawned by itself to be a machine, become it and
/// never come back ([`virtx_uvm::Machine`]).
pub fn become_machine_if_spawned() {
    if std::env::args_os()
        .nth(1)
        .is_some_and(|arg| arg == MACHINE_SUBCOMMAND)
    {
        // Why it could not be is the host's already, told on the control channel.
        let Err(_) = virtx_uvm::enter_machine();
        std::process::exit(1);
    }
}

/// The size a terminal is opened at, and the session it attaches to, if any.
#[derive(serde::Deserialize)]
struct Size {
    cols: Option<u16>,
    rows: Option<u16>,
    session: Option<String>,
}

impl Size {
    /// As a query asks for it: `cols=120&rows=40`.
    fn asked(query: &str) -> Size {
        let of = |name: &str| {
            query
                .split('&')
                .filter_map(|pair| pair.split_once('='))
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.to_owned())
        };
        Size {
            cols: of("cols").and_then(|v| v.parse().ok()),
            rows: of("rows").and_then(|v| v.parse().ok()),
            session: of("session").filter(|v| v.bytes().all(|b| b.is_ascii_alphanumeric())),
        }
    }
}

/// `GET /terminal/{sandbox}?cols=&rows=&session=`: a shell in the sandbox on a terminal, a job of
/// it, over a WebSocket of the subprotocol `virtx.terminal` -- kept across connections, as a
/// session ([`crate::terminals`]). Binary messages are the terminal's bytes, each way.
///
/// - It is first told `{"session": id, "attached": bool}`: the session's id, and whether it
///   attached to one named by `session`, which then sends what its shell wrote so far, before
///   what it writes from then on. A `session` that is not, or not this sandbox's, opens another.
/// - A text message to it, `{"cols": n, "rows": n}`, resizes it; `{"end": true}` kills the
///   shell, and ends the session.
/// - It ends with a text message from it, `{"exit": code}`, or `{"error": {code, message}}`
///   for one that did not run, or `taken` for a session another connection attached to since.
///   Closed otherwise, the shell runs on, for a connection to attach to again.
async fn terminal(
    access: &Access,
    sandbox: String,
    size: Size,
    headers: &HeaderMap,
    upgrade: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Response {
    if access.draining() {
        return draining();
    }
    let home = match access.home(headers).await {
        Ok((_, home)) => home,
        Err(status) => return refusal(status),
    };
    let Ok(upgrade) = upgrade else {
        return StatusCode::UPGRADE_REQUIRED.into_response();
    };
    let (cols, rows) = (
        size.cols.unwrap_or(80).max(2),
        size.rows.unwrap_or(24).max(2),
    );
    upgrade
        .protocols([TERMINAL_SUBPROTOCOL])
        .on_upgrade(move |mut socket| async move {
            let kept = size
                .session
                .as_deref()
                .and_then(|id| home.terminals.get(id, &sandbox));
            let attached = kept.is_some();
            let session = match kept {
                Some(session) => {
                    let _ = session.resize(cols, rows);
                    session
                }
                None => {
                    let cmd = vec!["sh".into(), "-c".into(), SHELL.into()];
                    match home.terminal(&sandbox, cmd, cols, rows).await {
                        Ok(opened) => home.terminals.start(&sandbox, opened),
                        Err(refused) => {
                            let said = serde_json::json!({ "error": refused.said() });
                            let _ = socket.send(Message::Text(said.to_string().into())).await;
                            let _ = socket.send(Message::Close(None)).await;
                            return;
                        }
                    }
                }
            };
            shell(socket, session, attached).await
        })
}

/// Carry a terminal's bytes between `socket` and the shell of `session` until the shell ends,
/// or the socket does -- detached, the shell left running -- or another attaches.
async fn shell(mut socket: WebSocket, session: Arc<Session>, attached: bool) {
    let (which, mut shown, written) = session.attach();
    let hello = serde_json::json!({ "session": session.id, "attached": attached });
    if socket
        .send(Message::Text(hello.to_string().into()))
        .await
        .is_err()
    {
        return session.detach(which);
    }
    if !written.is_empty()
        && socket
            .send(Message::Binary(written.into_bytes().into()))
            .await
            .is_err()
    {
        return session.detach(which);
    }
    // What it typed last, until the guest has written it: nothing more is read from it until
    // then, so a shell that does not read holds the connection back, while what it writes is
    // still shown.
    let mut typing: Option<std::pin::Pin<Box<dyn Future<Output = bool> + Send>>> = None;
    let said = loop {
        tokio::select! {
            typed = async { typing.as_mut().expect("typing").await }, if typing.is_some() => {
                typing = None;
                if !typed {
                    break None;
                }
            }
            out = shown.recv() => match out {
                Some(Shown::Data(data)) => {
                    if socket.send(Message::Binary(data.into_bytes().into())).await.is_err() {
                        break None;
                    }
                }
                Some(Shown::Exit(code)) => break Some(serde_json::json!({ "exit": code })),
                // Another connection attached to it since.
                None => break Some(serde_json::json!({ "error": {
                    "code": "taken",
                    "message": "the terminal is open in another window",
                } })),
            },
            received = socket.recv(), if typing.is_none() => match received {
                Some(Ok(Message::Binary(bytes))) => {
                    let (session, data) =
                        (Arc::clone(&session), String::from_utf8_lossy(&bytes).into_owned());
                    typing = Some(Box::pin(async move { session.write(data).await.is_ok() }));
                }
                Some(Ok(Message::Text(text))) => {
                    if let Ok(asked) = serde_json::from_str::<Asked>(&text) {
                        if asked.end {
                            session.end();
                        } else if let (Some(cols), Some(rows)) = (asked.cols, asked.rows) {
                            let _ = session.resize(cols.max(2), rows.max(2));
                        }
                    }
                }
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                // Closed: detached, the shell running on.
                Some(Ok(Message::Close(_)) | Err(_)) | None => break None,
            },
        }
    };
    session.detach(which);
    if let Some(said) = said {
        let _ = socket.send(Message::Text(said.to_string().into())).await;
    }
    let _ = socket.send(Message::Close(None)).await;
}

/// What a text message to a terminal asks: a size, or its end.
#[derive(serde::Deserialize)]
struct Asked {
    cols: Option<u16>,
    rows: Option<u16>,
    #[serde(default)]
    end: bool,
}

/// A request [`Access`] refused with `status`, as the endpoints refuse one.
fn refusal(status: StatusCode) -> Response {
    let (code, message) = match status {
        StatusCode::UNAUTHORIZED => ("unauthorized", "no token this server knows"),
        StatusCode::FORBIDDEN => ("forbidden", "not a member of the project, or not from here"),
        _ => (
            "invalid",
            "a request names its project in the virtx-project header",
        ),
    };
    endpoints::refused(status, code, message).into_response()
}

/// Whether `method` `path` stops or deletes a sandbox: what a draining server still takes, as
/// it ends what keeps the server from being let go.
fn lets_go(method: &Method, path: &str) -> bool {
    let Some(rest) = path.strip_prefix("/sandboxes/") else {
        return false;
    };
    match rest.split_once('/') {
        Some((id, "stop")) => *method == Method::POST && !id.is_empty(),
        None => *method == Method::DELETE && !rest.is_empty(),
        _ => false,
    }
}

/// What a draining server answers.
fn draining() -> Response {
    endpoints::refused(
        StatusCode::SERVICE_UNAVAILABLE,
        "draining",
        "this server takes no more requests",
    )
    .into_response()
}

/// The tag a registry request names a manifest by, as `name:tag`: what a storage home may
/// want fetched first.
fn tag_named(path: &str) -> Option<String> {
    let rest = path.strip_prefix("/images/oci/v2/")?;
    let (name, target) = rest.rsplit_once("/manifests/")?;
    // A digest is `algorithm:hex`; a tag has no colon.
    (!target.contains(':')).then(|| format!("{name}:{target}"))
}

/// What a registry request names by its digest: a manifest or a blob.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Part {
    Manifest,
    Blob,
}

/// The manifest or blob a registry request names by its digest, if it does.
fn digest_named(path: &str) -> Option<(Part, virtx_uvm::Digest)> {
    let rest = path.strip_prefix("/images/oci/v2/")?;
    let (part, target) = if let Some((_, target)) = rest.rsplit_once("/manifests/") {
        (Part::Manifest, target)
    } else {
        (Part::Blob, rest.rsplit_once("/blobs/")?.1)
    };
    Some((part, target.parse().ok()?))
}

/// `part` `digest` as `home`'s keeping has it, answered as the registry would: a manifest with
/// its media type, a blob as bytes -- the body only for a `GET`.
async fn kept(
    home: &endpoints::Home,
    part: Part,
    digest: &virtx_uvm::Digest,
    with_body: bool,
) -> anyhow::Result<Option<Response>> {
    let (body, media_type) = match part {
        Part::Manifest => {
            let Some(manifest) = home.kept_manifest(digest).await? else {
                return Ok(None);
            };
            let media_type = serde_json::from_str::<serde_json::Value>(&manifest)
                .ok()
                .and_then(|m| m["mediaType"].as_str().map(str::to_owned))
                .unwrap_or_else(|| "application/vnd.oci.image.manifest.v1+json".to_owned());
            (manifest.into_bytes(), media_type)
        }
        Part::Blob => {
            let Some(blob) = home.kept_blob(digest).await? else {
                return Ok(None);
            };
            (blob, "application/octet-stream".to_owned())
        }
    };
    let length = body.len().to_string();
    let headers = [
        (header::CONTENT_TYPE, media_type),
        (header::CONTENT_LENGTH, length),
        (
            header::HeaderName::from_static("docker-content-digest"),
            digest.to_string(),
        ),
    ];
    Ok(Some(if with_body {
        (StatusCode::OK, headers, body).into_response()
    } else {
        (StatusCode::OK, headers).into_response()
    }))
}

/// A `HEAD` of a manifest, as the registry answers it, for `manifest` by its `digest`.
fn head(digest: &virtx_uvm::Digest, manifest: &str) -> Response {
    let media_type = serde_json::from_str::<serde_json::Value>(manifest)
        .ok()
        .and_then(|m| m["mediaType"].as_str().map(str::to_owned))
        .unwrap_or_else(|| "application/vnd.oci.image.manifest.v1+json".to_owned());
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, media_type),
            (header::CONTENT_LENGTH, manifest.len().to_string()),
            (
                header::HeaderName::from_static("docker-content-digest"),
                digest.to_string(),
            ),
        ],
    )
        .into_response()
}

/// What a browser at one of the config's `origins` may do here: every method the endpoints
/// take, with the headers they read and the ones a client reads back. A preflight is answered
/// here, before any check, as a browser sends it without the token; the request it clears is
/// checked as any other.
fn cors(origins: &[String]) -> CorsLayer {
    let origins: Vec<HeaderValue> = origins
        .iter()
        .filter_map(|origin| origin.parse().ok())
        .collect();
    let method = |name| Method::from_bytes(name).expect("a method name");
    CorsLayer::new()
        .allow_origin(origins)
        .allow_methods([
            Method::GET,
            Method::HEAD,
            Method::POST,
            Method::PUT,
            Method::DELETE,
            Method::OPTIONS,
            method(b"PROPFIND"),
            method(b"MKCOL"),
            method(b"MOVE"),
            method(b"COPY"),
        ])
        .allow_headers([
            header::AUTHORIZATION,
            header::CONTENT_TYPE,
            header::IF_MATCH,
            header::RANGE,
            HeaderName::from_static(access::PROJECT_HEADER),
            HeaderName::from_static("depth"),
            HeaderName::from_static("destination"),
            HeaderName::from_static("overwrite"),
        ])
        .expose_headers([
            header::ETAG,
            header::CONTENT_RANGE,
            header::LOCATION,
            HeaderName::from_static("dav"),
        ])
        .max_age(Duration::from_secs(3600))
}

/// A request to the port `capability` opens: `403` from an `Origin` not let in, `426` unless it
/// opens a WebSocket, `400` unless of the subprotocol `virtx.tcp`, and `404` for a capability no
/// sandbox here has -- one never answered, or of a sandbox gone.
fn port(
    access: &Access,
    capability: &str,
    headers: &HeaderMap,
    upgrade: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Response {
    if let Err(status) = access.origin(headers) {
        return refusal(status);
    }
    let Ok(upgrade) = upgrade else {
        return StatusCode::UPGRADE_REQUIRED.into_response();
    };
    let offered = headers
        .get(header::SEC_WEBSOCKET_PROTOCOL)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(',')
                .any(|offered| offered.trim() == TCP_SUBPROTOCOL)
        });
    if !offered {
        return (
            StatusCode::BAD_REQUEST,
            format!("a port is reached over the WebSocket subprotocol {TCP_SUBPROTOCOL}"),
        )
            .into_response();
    }
    let Some(tunnel) = access
        .homes()
        .iter()
        .find_map(|home| home.tunnel(capability))
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    upgrade
        .protocols([TCP_SUBPROTOCOL])
        .on_upgrade(move |socket| relay(socket, tunnel))
}

/// Carry bytes between `socket` and the port `tunnel` reaches until either ends: what is
/// read from the port as binary messages, and the binary messages to the port. Closed at once
/// when no machine runs, as a port nothing listens on is.
async fn relay(mut socket: WebSocket, tunnel: Tunnel) {
    let reached = tunnel.connect().await;
    if reached.is_err() {
        return tunnel.refused();
    }
    if let Ok(mut reached) = reached {
        let (mut from, mut to) = reached.split();
        let mut buffer = vec![0u8; CHUNK];
        loop {
            tokio::select! {
                read = from.read(&mut buffer) => match read {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let message = Message::Binary(buffer[..n].to_vec().into());
                        if socket.send(message).await.is_err() {
                            break;
                        }
                    }
                },
                received = socket.recv() => match received {
                    Some(Ok(Message::Binary(bytes))) => {
                        if to.write_all(&bytes).await.is_err() {
                            break;
                        }
                    }
                    // Answered by the socket itself.
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                    // Text is no TCP's, and anything else is the end.
                    Some(Ok(Message::Text(_) | Message::Close(_)) | Err(_)) | None => break,
                },
            }
        }
    }
    let _ = socket.send(Message::Close(None)).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_registry_request_names_a_manifest_by_its_tag() {
        assert!(lets_go(&Method::POST, "/sandboxes/sbx_1/stop"));
        assert!(lets_go(&Method::DELETE, "/sandboxes/sbx_1"));
        assert!(!lets_go(&Method::POST, "/sandboxes/sbx_1/start"));
        assert!(!lets_go(&Method::GET, "/sandboxes/sbx_1"));
        assert!(!lets_go(&Method::POST, "/sandboxes/"));
        assert!(!lets_go(&Method::DELETE, "/sandboxes/sbx_1/jobs/j"));
        let digest = "sha256:".to_owned() + &"ab".repeat(32);
        assert_eq!(
            digest_named(&format!("/images/oci/v2/team/app/manifests/{digest}")),
            Some((Part::Manifest, digest.parse().unwrap()))
        );
        assert_eq!(
            digest_named(&format!("/images/oci/v2/app/blobs/{digest}")),
            Some((Part::Blob, digest.parse().unwrap()))
        );
        assert_eq!(digest_named("/images/oci/v2/app/manifests/v1"), None);
        assert_eq!(digest_named("/images/oci/v2/app/blobs/uploads/"), None);
        let named = |p: &str| tag_named(p);
        assert_eq!(named("/images/oci/v2/_catalog"), None);
        assert_eq!(named("/images/oci/v2/team/app/tags/list"), None);
        assert_eq!(
            named("/images/oci/v2/team/app/manifests/v1"),
            Some("team/app:v1".into())
        );
        assert_eq!(named("/images/oci/v2/app/manifests/sha256:ab12"), None);
        assert_eq!(named("/images/oci/v2/app/blobs/sha256:ab12"), None);
        assert_eq!(named("/images/oci/v2/"), None);
    }
}
