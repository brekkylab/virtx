//! `virtx-server-tcp` -- a virtx server over HTTP, to the users and projects its config names
//! (see [`config`]): each request carries a user's token and names a project, and the endpoints
//! `virtx-core`'s `PROTOCOL.md` names answer it from that project's home.
//!
//! The images every project shares are at `/images/oci/v2/`, as an OCI registry (see
//! [`virtx_uvm::RegistryServer`]), to any user.
//!
//! A sandbox's ports are at `/ports/<capability>` beside the endpoints, each a WebSocket of the
//! subprotocol `virtx.tcp` carrying a TCP connection to the port: the capability, which only
//! making the sandbox answers, is all a connection needs, and gives no more than the one port.
//!
//! `virtx-server-tcp token` makes a token, and says what the config keeps of it.
//! `virtx-server-tcp vm …` is this binary spawned by itself to be a machine; see
//! [`virtx_uvm::Machine`].

mod access;
mod config;
mod endpoints;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use axum::extract::Path;
use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::middleware::Next;
use axum::response::{IntoResponse as _, Response};
use axum::routing::any;
use clap::Parser;
use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tower_http::cors::CorsLayer;
use virtx_uvm::RegistryServer;

use crate::access::Access;
use crate::config::Config;
use crate::endpoints::Tunnel;

/// The WebSocket subprotocol a port's TCP is carried in.
const TCP_SUBPROTOCOL: &str = "virtx.tcp";

/// How much of a port's TCP one message carries at most.
const CHUNK: usize = 64 << 10;

/// What this binary is spawned with to be a machine.
const MACHINE_SUBCOMMAND: &str = "vm";

/// How often the images every project shares are pruned, as `POST /images/prune` does.
const PRUNE_EVERY: Duration = Duration::from_secs(60 * 60);

/// How long a sandbox is kept with no job running in it: over HTTP no client is ever known to
/// be gone, so one that stops naming its sandboxes is taken to be, and what they hold -- their
/// snapshots -- let go.
const IDLE: Duration = Duration::from_secs(15 * 60);

#[derive(Parser)]
enum Command {
    /// Serve the endpoints, and sandboxes' ports beside them, as `--config` says.
    Serve {
        #[arg(long)]
        config: PathBuf,
    },
    /// Make a token for a user, and its `token_sha256` for the config.
    Token,
}

fn main() -> anyhow::Result<()> {
    // Before any runtime: this process is about to become a machine and never come back.
    if std::env::args_os()
        .nth(1)
        .is_some_and(|arg| arg == MACHINE_SUBCOMMAND)
    {
        // Why it could not be is the host's already, told on the control channel; stderr is
        // the guest command's, and nothing of this process's goes there.
        let Err(_) = virtx_uvm::enter_machine();
        std::process::exit(1);
    }
    match Command::parse() {
        Command::Serve { config } => serve(Config::read(&config)?),
        Command::Token => {
            let token = format!(
                "{}{}",
                uuid::Uuid::new_v4().simple(),
                uuid::Uuid::new_v4().simple()
            );
            println!("token:        {token}");
            println!("token_sha256: {}", config::hashed(&token));
            Ok(())
        }
    }
}

#[tokio::main]
async fn serve(config: Config) -> anyhow::Result<()> {
    let access = Arc::new(Access::new(&config)?);
    let registry = Arc::new(RegistryServer::new(access.images().clone(), "/images/oci")?);
    let reading = Arc::clone(&access);
    let images = move |request: axum::extract::Request| {
        let (registry, access) = (Arc::clone(&registry), Arc::clone(&reading));
        async move {
            match access.user(request.headers()) {
                Ok(_) => registry
                    .handle(request)
                    .await
                    .map(axum::body::Body::new)
                    .into_response(),
                Err(status) => refusal(status),
            }
        }
    };
    let checked = Arc::clone(&access);
    let reaching = Arc::clone(&access);
    let router = endpoints::router()
        .layer(axum::middleware::from_fn(
            move |mut request: axum::extract::Request, next: Next| {
                let home = checked.home(request.headers());
                async move {
                    match home {
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
        .layer(cors(&config.origins));

    let reaping = Arc::clone(&access);
    tokio::spawn(async move {
        let mut every = tokio::time::interval(Duration::from_secs(60));
        loop {
            every.tick().await;
            for home in reaping.homes() {
                home.forget_idle(IDLE);
            }
        }
    });

    let images = access.images().clone();
    tokio::spawn(async move {
        let mut every = tokio::time::interval(PRUNE_EVERY);
        loop {
            every.tick().await;
            let images = images.clone();
            match tokio::task::spawn_blocking(move || images.prune(endpoints::PRUNE_GRACE)).await {
                Ok(Ok(_)) => {}
                Ok(Err(e)) => eprintln!("pruning the images: {e}"),
                Err(e) => eprintln!("pruning the images: {e}"),
            }
        }
    });

    let listener = tokio::net::TcpListener::bind(config.listen)
        .await
        .with_context(|| format!("listening at {}", config.listen))?;
    eprintln!("serving at http://{}/", listener.local_addr()?);
    // A machine still running at the end shuts down with this process, as its control channel
    // closes.
    axum::serve(listener, router)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
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
    let Some(tunnel) = access.homes().find_map(|home| home.tunnel(capability)) else {
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
    if let Ok(mut reached) = tunnel.connect().await {
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
