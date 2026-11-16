//! The server's routes and loops, for an [`Access`]: the endpoints of `virtx-core`'s
//! `PROTOCOL.md` ([`crate::endpoints`]), each answered from the home its request reaches, the
//! images as an OCI registry, and each sandbox's ports; and sandboxes stopped once unused and
//! deleted once kept past their days, images pruned. A server built on this one -- virtx
//! cloud's node -- gives [`router`] an access of its own, and routes of its own beside these;
//! the server a config alone makes is [`configured`], which does both.
//!
//! A project's images are at `/images/oci/v2/`, as an OCI registry (see
//! [`virtx_uvm::RegistryServer`]): its own, to whoever reaches the project.
//!
//! A sandbox's ports are at `/ports/<capability>` beside the endpoints, each a WebSocket of the
//! subprotocol `virtx.tcp` carrying a TCP connection to the port: the capability, which only
//! making the sandbox answers, is all a connection needs, and gives no more than the one port.
//!
//! A shell in a sandbox, on a terminal, is at `/terminal/{sandbox}`: a WebSocket of the
//! subprotocol `virtx.terminal` ([`terminal`]), for the same callers as the endpoints, kept
//! across connections ([`crate::terminals`]).

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

use crate::access::{self, Access, Homes, Projects};
use crate::config::Config;
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

/// How often each project's images are pruned, as `POST /images/prune` does.
const PRUNE_EVERY: Duration = Duration::from_secs(60 * 60);

/// The server `config` alone makes: its [`Projects`] behind [`router`], and beside it where a
/// client signs in for its tokens, answered without any -- the provider and the client to
/// sign in as, or nothing where no one signs in.
pub fn configured(config: &Config) -> anyhow::Result<(Arc<Projects>, Arc<Access>, axum::Router)> {
    let homes = Arc::new(Homes::new(config.home().join("project")));
    let projects = Arc::new(Projects::new(config, &homes)?);
    let advertised = match &config.oidc {
        Some(oidc) => serde_json::json!({
            "oidc": { "issuer": oidc.issuer, "client_id": oidc.client_id },
        }),
        None => serde_json::json!({}),
    };
    let advertised = (
        [(header::CONTENT_TYPE, "application/json")],
        advertised.to_string(),
    );
    let own = axum::Router::new().route(
        "/auth",
        axum::routing::get(move || async move { advertised }),
    );
    let access = Arc::new(Access::new(config.origins.clone(), projects.clone(), homes));
    let router = router(Arc::clone(&access), &config.origins, own);
    Ok((projects, access, router))
}

/// The routes, for `access`, a browser at one of `origins` let in, with `own` beside them:
/// a server built on this one's, which take no home.
pub fn router(access: Arc<Access>, origins: &[String], own: axum::Router) -> axum::Router {
    let registry = format!("{}/v2", access::REGISTRY_PATH);
    let checked = Arc::clone(&access);
    let reaching = Arc::clone(&access);
    let reading = Arc::clone(&access);
    let opening = Arc::clone(&access);

    let images = move |request: axum::extract::Request| {
        let access = Arc::clone(&reading);
        async move {
            match access.home(request.headers()).await {
                Ok((_, home)) => {
                    let registry = home.registry();
                    // A tag named to be pulled is brought up to the storage home's index first:
                    // its image fetched, if it is not here yet. A `HEAD` of it fetches nothing:
                    // the storage home's manifest answers. A listing lists the tags still to
                    // be fetched too (the registry's `listing_too`).
                    if let Some(reference) = tag_named(request.uri().path()) {
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
                    let by_digest = digest_named(request.uri().path()).filter(|_| {
                        matches!(*request.method(), http::Method::GET | http::Method::HEAD)
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
                        && let Some((part, digest)) = by_digest
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
        .route(&registry, any(images.clone()))
        .route(&format!("{registry}/"), any(images.clone()))
        .route(&format!("{registry}/{{*rest}}"), any(images))
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
            // Each project's own: a store pruned once however many homes share it.
            let mut stores: Vec<virtx_uvm::ImageStore> = Vec::new();
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
    //! The app, served on the loopback over a config of its own that authenticates no one, and
    //! called over HTTP as a client would: each request to an endpoint in a project -- by
    //! hand, and through `virtx-core`'s client given it at connect. Who reaches which project
    //! is `virtx-rs`'s `tests/projects.rs`, against the fixture.

    use std::path::PathBuf;
    use std::sync::Arc;

    use reqwest::{Method, StatusCode, Url};
    use serde_json::{Value, json};
    use sha2::{Digest as _, Sha256};
    use tokio_tungstenite::tungstenite;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
    use virtx_core::{Auth, Client, DirEntry, FileRoot, SandboxCreateArgs};
    use virtx_uvm::{FsTree, ImageStore};

    use super::{Part, digest_named, tag_named};
    use crate::config::Config;

    type Socket = tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >;

    /// The app served on the loopback over an empty home, stopped and removed when this drops.
    struct Served {
        task: tokio::task::JoinHandle<std::io::Result<()>>,
        /// Its base URL, ending in `/`.
        url: String,
        home: PathBuf,
    }

    impl Served {
        /// Of the projects `demo` and `other`.
        async fn spawn(name: &str) -> Served {
            Served::spawn_with(name, json!({ "projects": { "demo": {}, "other": {} } })).await
        }

        /// With `who` in the config, for who is let in and to which projects.
        async fn spawn_with(name: &str, who: Value) -> Served {
            let home =
                std::env::temp_dir().join(format!("virtx-tcp-app-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&home);
            std::fs::create_dir_all(&home).unwrap();
            let mut config = json!({
                "listen": "127.0.0.1:0",
                "home": home,
                "origins": ["https://allowed.example"],
            });
            for (name, value) in who.as_object().unwrap() {
                config[name] = value.clone();
            }
            let path = home.join("config.json");
            std::fs::write(&path, config.to_string()).unwrap();
            let config = Config::read(&path).unwrap();

            let (_, _, router) = super::configured(&config).unwrap();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}/", listener.local_addr().unwrap());
            let task = tokio::spawn(async move { axum::serve(listener, router).await });
            Served { task, url, home }
        }

        /// `method` sent to `path` in `project`: the HTTP status, and the body.
        async fn send(
            &self,
            method: &str,
            path: &str,
            project: Option<&str>,
            body: &'static str,
        ) -> (StatusCode, String) {
            let method = Method::from_bytes(method.as_bytes()).unwrap();
            let mut request = reqwest::Client::new()
                .request(method, format!("{}{path}", self.url))
                .body(body);
            if let Some(project) = project {
                request = request.header("virtx-project", project);
            }
            let response = request.send().await.unwrap();
            let status = response.status();
            (status, response.text().await.unwrap())
        }

        /// A client in `demo`.
        fn connect(&self) -> Client {
            let mut headers = http::HeaderMap::new();
            headers.insert("virtx-project", "demo".parse().unwrap());
            Client::connect(&self.url, Auth::None, headers).unwrap()
        }

        /// File an empty image under `tag` in `demo`'s images.
        fn file_image(&self, tag: &str) {
            let store = ImageStore::new(self.home.join("project/demo/images")).unwrap();
            let layer = store.insert_layer(&FsTree::new()).unwrap();
            let config = serde_json::from_str(
                r#"{"architecture": "arm64", "os": "linux", "rootfs": {"type": "layers", "diff_ids": []}}"#,
            )
            .unwrap();
            let image = store.insert_image(&config, &[layer]).unwrap();
            store.insert_tag(&tag.parse().unwrap(), &image).unwrap();
        }

        /// A port's URL, as a client resolves what making its sandbox answered: against the URL
        /// it called, as a WebSocket's.
        fn port_url(&self, url: &str) -> Url {
            let mut url = Url::parse(&self.url).unwrap().join(url).unwrap();
            url.set_scheme("ws").unwrap();
            url
        }

        /// A WebSocket to `url`, offering `protocol`: the socket, or the status it was refused with.
        async fn open(url: &Url, protocol: Option<&str>) -> Result<Socket, u16> {
            let mut request = url.as_str().into_client_request().unwrap();
            if let Some(protocol) = protocol {
                request
                    .headers_mut()
                    .insert("sec-websocket-protocol", protocol.parse().unwrap());
            }
            match tokio_tungstenite::connect_async(request).await {
                Ok((socket, _)) => Ok(socket),
                Err(tungstenite::Error::Http(response)) => Err(response.status().as_u16()),
                Err(e) => panic!("{e}"),
            }
        }
    }

    impl Drop for Served {
        fn drop(&mut self) {
            self.task.abort();
            let _ = std::fs::remove_dir_all(&self.home);
        }
    }

    /// The HTTP status a request was refused with, whatever the body it came with.
    fn status<T: std::fmt::Debug>(answered: Result<T, virtx_core::Error>) -> u16 {
        match answered {
            Err(e) => e
                .status()
                .unwrap_or_else(|| panic!("not refused by the server: {e}")),
            Ok(answer) => panic!("expected a refusal, got {answer:?}"),
        }
    }

    fn names(entries: Vec<DirEntry>) -> Vec<String> {
        entries.into_iter().map(|entry| entry.name).collect()
    }

    #[tokio::test]
    async fn a_server_says_where_to_sign_in_and_without_authentication_takes_any_request() {
        let asked = async |served: &Served| {
            let (status, body) = served.send("GET", "auth", None, "").await;
            assert_eq!(status, StatusCode::OK);
            serde_json::from_str::<Value>(&body).unwrap()
        };

        let oidc = json!({ "issuer": "https://idp.example", "client_id": "virtx" });
        let signed_in = Served::spawn_with("advertised", json!({ "oidc": oidc })).await;
        assert_eq!(asked(&signed_in).await, json!({ "oidc": oidc }));
        let list = signed_in.send("GET", "volumes/", None, "").await;
        assert_eq!(list.0, StatusCode::UNAUTHORIZED);

        let open = Served::spawn_with("open", json!({})).await;
        assert_eq!(asked(&open).await, json!({}));
        let list = open.send("GET", "volumes/", None, "").await;
        assert_eq!((list.0, list.1.as_str()), (StatusCode::OK, "[]"));
    }

    #[tokio::test]
    async fn a_browser_is_let_in_from_the_config_s_origins_alone() {
        let served = Served::spawn("origins").await;
        let from = async |origin| {
            reqwest::Client::new()
                .get(format!("{}volumes/", served.url))
                .header("virtx-project", "demo")
                .header("origin", origin)
                .send()
                .await
                .unwrap()
                .status()
        };
        assert_eq!(from("https://allowed.example").await, StatusCode::OK);
        assert_eq!(from("https://evil.example").await, StatusCode::FORBIDDEN);

        // A browser's preflight is answered for the WebDAV methods from
        // an origin let in, and left without its answer from any other.
        let files = format!("{}volumes/data/files/", served.url);
        let preflight = async |origin| {
            reqwest::Client::new()
                .request(Method::OPTIONS, &files)
                .header("origin", origin)
                .header("access-control-request-method", "PROPFIND")
                .header(
                    "access-control-request-headers",
                    "authorization, virtx-project, depth",
                )
                .send()
                .await
                .unwrap()
        };
        let cleared = preflight("https://allowed.example").await;
        assert!(cleared.status().is_success(), "{}", cleared.status());
        let header = |name| {
            cleared
                .headers()
                .get(name)
                .map(|value| value.to_str().unwrap().to_owned())
        };
        assert_eq!(
            header("access-control-allow-origin").as_deref(),
            Some("https://allowed.example")
        );
        let methods = header("access-control-allow-methods").unwrap();
        assert!(
            methods.contains("PROPFIND") && methods.contains("MKCOL"),
            "{methods}"
        );
        let headers = header("access-control-allow-headers")
            .unwrap()
            .to_lowercase();
        assert!(
            headers.contains("depth") && headers.contains("virtx-project"),
            "{headers}"
        );
        let refused = preflight("https://evil.example").await;
        assert!(
            refused
                .headers()
                .get("access-control-allow-origin")
                .is_none()
        );

        // The request it clears carries the origin's answer too.
        served.connect().create_volume(Some("data")).await.unwrap();
        let listed = reqwest::Client::new()
            .request(Method::from_bytes(b"PROPFIND").unwrap(), &files)
            .header("virtx-project", "demo")
            .header("origin", "https://allowed.example")
            .header("depth", "1")
            .send()
            .await
            .unwrap();
        assert_eq!(listed.status(), StatusCode::MULTI_STATUS);
        assert_eq!(
            listed.headers().get("access-control-allow-origin").unwrap(),
            "https://allowed.example"
        );
    }

    /// A project's images are an OCI registry at `/images/oci/v2/`, and no other project's.
    #[tokio::test]
    async fn a_project_s_images_are_a_registry() {
        let served = Served::spawn("registry").await;
        served.file_image("alpine:3.21");
        let base = format!("{}images/oci/v2", served.url);
        let get = async |path: &str, project: &str| {
            reqwest::Client::new()
                .get(format!("{base}/{path}"))
                .header("virtx-project", project)
                .send()
                .await
                .unwrap()
        };
        assert_eq!(get("", "demo").await.status(), StatusCode::OK);

        let manifest = get("alpine/manifests/3.21", "demo").await;
        assert_eq!(manifest.status(), StatusCode::OK);
        let manifest: Value = manifest.json().await.unwrap();
        let layer = &manifest["layers"][0];
        assert_eq!(layer["mediaType"], "application/vnd.erofs.layer.v1");
        let digest = layer["digest"].as_str().unwrap();
        let blob = get(&format!("alpine/blobs/{digest}"), "demo").await;
        assert_eq!(blob.status(), StatusCode::OK);
        assert_eq!(
            format!("sha256:{:x}", Sha256::digest(blob.bytes().await.unwrap())),
            layer["digest"]
        );

        // Another project has none of them, by tag or by digest.
        assert_eq!(get("", "other").await.status(), StatusCode::OK);
        let theirs = get("alpine/manifests/3.21", "other").await;
        assert_eq!(theirs.status(), StatusCode::NOT_FOUND);
        let theirs = get(&format!("alpine/blobs/{digest}"), "other").await;
        assert_eq!(theirs.status(), StatusCode::NOT_FOUND);
    }

    /// The files of a volume through `virtx-core`'s client, given the project at connect, as a
    /// program over the bindings reaches them.
    #[tokio::test]
    async fn the_client_reaches_a_project_with_the_headers_given_at_connect() {
        let served = Served::spawn("client").await;
        let client = served.connect();
        client.create_volume(Some("data")).await.unwrap();
        let volume = FileRoot::Volume("data".into());
        let home = served.home.join("project/demo");

        // A volume's files, on the host's disk.
        client.create_dir(&volume, "dir").await.unwrap();
        client
            .write_file(&volume, "dir/a.txt", "hello world", None)
            .await
            .unwrap();
        assert_eq!(
            std::fs::read(home.join("volume/data/files/dir/a.txt")).unwrap(),
            b"hello world"
        );
        let metadata = client.stat_file(&volume, "dir/a.txt").await.unwrap();
        assert!(!metadata.is_dir && metadata.size == 11, "{metadata:?}");
        assert_eq!(
            names(client.read_dir(&volume, "dir").await.unwrap()),
            ["a.txt"]
        );
        assert_eq!(names(client.read_dir(&volume, "").await.unwrap()), ["dir"]);
        let read = client
            .read_file(&volume, "dir/a.txt", 6, Some(3))
            .await
            .unwrap();
        assert_eq!(&read[..], b"wor");
        client
            .rename_file(&volume, "dir/a.txt", "b.txt", false)
            .await
            .unwrap();
        client
            .copy_file(&volume, "b.txt", "c.txt", false)
            .await
            .unwrap();
        assert_eq!(
            status(client.copy_file(&volume, "b.txt", "c.txt", false).await),
            412
        );
        client
            .copy_file(&volume, "dir", "copied", false)
            .await
            .unwrap();
        client.remove_file(&volume, "dir", false).await.unwrap();
        assert_eq!(status(client.stat_file(&volume, "dir").await), 404);
        assert!(client.stat_file(&volume, "copied").await.unwrap().is_dir);
        let read = client.read_file(&volume, "c.txt", 0, None).await.unwrap();
        assert_eq!(&read[..], b"hello world");
        assert_eq!(
            status(client.write_file(&volume, "no/file", "x", None).await),
            409
        );
        let nobody = FileRoot::Volume("nobody".into());
        assert_eq!(status(client.stat_file(&nobody, "").await), 404);
    }

    #[tokio::test]
    async fn a_project_keeps_its_volumes() {
        let served = Served::spawn("project").await;
        let client = served.connect();
        client.create_volume(Some("data")).await.unwrap();
        assert!(served.home.join("project/demo/volume/data/files").is_dir());
        assert_eq!(status(client.create_volume(Some("data")).await), 409);

        // A port that is none, and a sandbox of an image nobody filed, are refused before it
        // boots.
        let mut args = SandboxCreateArgs::new("a:b");
        args.ports = vec![0.into()];
        assert_eq!(status(client.create_sandbox(&args).await), 400);
        let args = SandboxCreateArgs::new("a:b");
        assert_eq!(status(client.create_sandbox(&args).await), 404);
    }

    /// A stand-in for an embedder's registry proxy, which refuses whatever it is asked, saying so.
    struct Refusing {
        url: String,
    }

    struct RefusingPass {
        url: String,
        refused: std::sync::Mutex<bool>,
    }

    impl virtx_uvm::RegistryProxy for Refusing {
        fn pass(&self) -> Box<dyn virtx_uvm::ProxyPass> {
            Box::new(RefusingPass {
                url: self.url.clone(),
                refused: std::sync::Mutex::new(false),
            })
        }
    }

    impl virtx_uvm::ProxyPass for RefusingPass {
        fn url(&self) -> &str {
            &self.url
        }

        fn refused(&self) -> Option<virtx_uvm::ProxyRefusal> {
            let mut refused = self.refused.lock().unwrap();
            (!std::mem::replace(&mut *refused, true)).then(|| virtx_uvm::ProxyRefusal {
                host: "the registry".into(),
                why: "it is not let through".into(),
            })
        }
    }

    /// A server given a registry proxy, as virtx cloud's nodes are: a pull goes through it --
    /// what it refuses fails saying so -- and connects to the registry itself not at all.
    #[tokio::test]
    async fn a_server_s_pulls_go_through_its_registry_proxy() {
        use futures_util::StreamExt as _;
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        use virtx_core::ImageJobEntry;

        let served = Served::spawn("registry-proxy").await;
        let proxy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", proxy.local_addr().unwrap());
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = proxy.accept().await {
                let mut head = [0; 1024];
                let _ = stream.read(&mut head).await;
                let _ = stream
                    .write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n")
                    .await;
            }
        });
        crate::endpoints::set_registry_proxy(Arc::new(Refusing { url }));
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let client = served.connect();

        let reference = format!("127.0.0.1:{port}/team/app:1");
        let job = client.pull_image(&reference, None).await.unwrap();
        let mut log = client.image_job_log(&job.id, None);
        // A guard against a hang, not a measure of anything.
        let failed = tokio::time::timeout(std::time::Duration::from_secs(60), async {
            loop {
                match log.next().await.unwrap() {
                    Ok(entry) => assert!(!matches!(entry.entry, ImageJobEntry::End(_))),
                    Err(failed) => break failed,
                }
            }
        })
        .await
        .unwrap();
        let virtx_core::Error::Job(why) = failed else {
            panic!("{failed:?}");
        };
        assert_eq!(
            why,
            format!(
                "{reference}: the registry is not reachable from this server: \
                 it is not let through"
            )
        );
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock,
            "the registry was connected to"
        );
    }

    /// A port is a WebSocket of `virtx.tcp` and nothing else, behind a capability no one guesses.
    #[tokio::test]
    async fn a_port_is_a_websocket_behind_its_capability() {
        let served = Served::spawn("ports").await;
        let guessed = served.port_url(&format!("ports/{}", "0".repeat(64)));
        let mut plain = guessed.clone();
        plain.set_scheme("http").unwrap();
        let elsewhere = reqwest::Client::new()
            .get(plain.clone())
            .header("origin", "https://elsewhere.example")
            .send()
            .await
            .unwrap();
        assert_eq!(elsewhere.status(), StatusCode::FORBIDDEN);
        let plain = reqwest::get(plain).await.unwrap();
        assert_eq!(plain.status(), StatusCode::UPGRADE_REQUIRED);
        assert_eq!(Served::open(&guessed, None).await.err(), Some(400));
        assert_eq!(
            Served::open(&guessed, Some("virtx.tcp")).await.err(),
            Some(404)
        );
    }

    /// Two clients writing over one etag at once: the first to write lands, and the other is
    /// refused the etag it read, gone stale under it.
    #[tokio::test]
    async fn of_two_writes_over_one_etag_one_lands() {
        let served = Served::spawn("etag").await;
        let (a, b) = (served.connect(), served.connect());
        a.create_volume(Some("data")).await.unwrap();
        let root = FileRoot::Volume("data".into());
        let (from_a, from_b) = (vec![b'a'; 1 << 20], vec![b'b'; 1 << 20]);
        for round in 0..20 {
            a.write_file(&root, "read", format!("round {round}"), None)
                .await
                .unwrap();
            let etag = b.stat_file(&root, "read").await.unwrap().etag.unwrap();
            let (by_a, by_b) = tokio::join!(
                a.write_file(&root, "read", from_a.clone(), Some(&etag)),
                b.write_file(&root, "read", from_b.clone(), Some(&etag)),
            );
            let landed = match (by_a, by_b) {
                (Ok(()), Err(virtx_core::Error::Status(412, _))) => &from_a,
                (Err(virtx_core::Error::Status(412, _)), Ok(())) => &from_b,
                (by_a, by_b) => panic!("round {round}, over one etag: {by_a:?} and {by_b:?}"),
            };
            let read = a.read_file(&root, "read", 0, None).await.unwrap();
            assert!(read == *landed, "round {round}: not the write that landed");
        }
    }

    #[test]
    fn a_registry_request_names_a_manifest_by_its_tag() {
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
