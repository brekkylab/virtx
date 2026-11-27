//! A sandbox's port forwarded by this client, on either side of the sandbox.
//!
//! A port in the sandbox is reached at an address on this machine. A server on this machine
//! answers each such port's loopback address, its `url`, which is reached as it is, or through a
//! listener of the forwarder's own where another address is asked for. A server over HTTP is
//! asked for `/sandboxes/{id}/ports/{port}` as a WebSocket of the subprotocol `virtx.tcp`: so
//! each connection to the forwarder's address is carried in one, its bytes as binary messages,
//! opened by the same HTTP client the requests go by, with their credentials and project.
//!
//! A port on local is reached by the sandbox's processes at `10.0.2.2`, each of their
//! connections carried to the forwarder's target on this machine. A server over HTTP is held a
//! WebSocket of `virtx.tcp` at `/sandboxes/{id}/client-ports/{port}` while the port is taken. A
//! process's connection to the port is said on it with an empty binary message: the target is
//! connected to then, and the connection answered with another empty message once it is
//! reached, or the WebSocket closed when it is not, which refuses it. Each WebSocket carries one
//! connection, and another is opened to wait for the next as soon as one is said. A server on
//! this machine, over stdio, reaches the target itself: it is held `GET` of the same path, with
//! the target in `?target=`, while the port is taken, its answer's body ending once the sandbox
//! is deleted.
//!
//! A connection that fails -- its WebSocket refused, or closed before the port sent a byte; its
//! target not reached -- is closed, and what was seen is kept as the forwarder's
//! [last error](PortForwarder::last_error) until a connection reaches the port, and warned of,
//! naming the port.

#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use std::{
    convert::Infallible,
    future::Future,
    net::{SocketAddr, ToSocketAddrs},
    ops::Deref,
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};

use super::{
    Body, Connection, TransportClient,
    credentials::Credentials,
    tcp::{Shared, authorized, current},
};
use crate::{
    Error, PROJECT_HEADER, PortSide,
    transport::{full, http::refused, segment},
    warning,
};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use http::{HeaderMap, HeaderValue, StatusCode, header};
use http_body_util::BodyExt;
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::watch,
    task::JoinSet,
};
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{
        self, Message,
        handshake::{client::generate_key, derive_accept_key},
        protocol::Role,
    },
};

/// The WebSocket subprotocol a port's TCP is carried in.
const TCP_SUBPROTOCOL: &str = "virtx.tcp";

/// How much of a port's TCP one message carries at most.
const CHUNK: usize = 64 << 10;

/// How long reaching what a connection is carried to may take before it is closed.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Why a connection through a forwarder failed: its WebSocket refused, the server not reached,
/// the WebSocket closed before the port sent a byte, with the reason the server gave if it gave
/// one, or what it is carried to not reached.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ForwardError {
    /// For a person: the port, and what the server answered.
    pub message: String,
    /// Where it failed: the server, where it refused the WebSocket; the transport otherwise.
    pub source: crate::Source,
    /// The HTTP status the server refused the WebSocket with, if it did.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    /// The protocol's code it refused with, where it answered one: `not_found` and the like.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

impl ForwardError {
    fn new(message: impl Into<String>) -> ForwardError {
        ForwardError {
            message: message.into(),
            source: crate::Source::Transport,
            status: None,
            code: None,
        }
    }
}

/// A forwarder's failures: the port they are of, as a person knows it, and the last since a
/// connection last reached the port.
struct Failures {
    port: String,
    last: Mutex<Option<ForwardError>>,
}

impl Failures {
    fn new(port: String) -> Arc<Failures> {
        Arc::new(Failures {
            port,
            last: Mutex::default(),
        })
    }

    /// Keep `failed`, named for the port, as the last, and warn of it.
    fn fail(&self, failed: ForwardError) {
        let failed = ForwardError {
            message: format!("{}: {}", self.port, failed.message),
            ..failed
        };
        warning::warn(failed.message.clone());
        *self.last.lock().unwrap_or_else(PoisonError::into_inner) = Some(failed);
    }

    /// A connection reached the port: what failed before is no longer the port's state.
    fn reached(&self) {
        *self.last.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }
}

/// How long the forwarder waits before accepting again after `error`: not at all where it is
/// one connection's, reset before it was taken, and a second where it is the process's, such as
/// running out of descriptors, which taking the next one at once would meet again.
fn accept_backoff(error: &std::io::Error) -> Option<Duration> {
    use std::io::ErrorKind::*;
    match error.kind() {
        ConnectionAborted | ConnectionRefused | ConnectionReset | Interrupted | WouldBlock => None,
        _ => Some(Duration::from_secs(1)),
    }
}

/// What opening a forwarder takes: the sandbox's port, on its side, and the forwarder's local
/// end.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PortForwarderArgs {
    /// The sandbox, by id.
    pub sandbox: String,

    /// The port, one the sandbox was made with on the side `on`.
    pub port: u16,

    /// Which side the port is on.
    #[serde(default)]
    pub on: PortSide,

    /// The port's `url`, as a server on this machine answers a port in the sandbox.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,

    /// The local end, `host:port`: for a port in the sandbox, the address it is reached at,
    /// `127.0.0.1:0` without one, so a free port; for a port on local, the target each of the
    /// sandbox's connections to it is carried to, `127.0.0.1:{port}` without one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local: Option<String>,
}

impl PortForwarderArgs {
    /// The port `port` in the sandbox `sandbox`, its local end left to the forwarder.
    pub fn new(sandbox: impl Into<String>, port: u16) -> Self {
        PortForwarderArgs {
            sandbox: sandbox.into(),
            port,
            on: PortSide::Sandbox,
            url: None,
            local: None,
        }
    }

    pub fn on(mut self, on: PortSide) -> Self {
        self.on = on;
        self
    }

    pub fn url(mut self, url: impl Into<String>) -> Self {
        self.url = Some(url.into());
        self
    }

    pub fn local(mut self, local: impl Into<String>) -> Self {
        self.local = Some(local.into());
        self
    }
}

/// Where a forwarder is: opening, open at its local end, or ended by what ended it.
#[derive(Clone)]
enum Phase {
    Opening,
    Open(String),
    Ended(Error),
}

/// A sandbox's port, on either side, forwarded for as long as what [`open`](Self::open) answers
/// with it runs: one in the sandbox reached at its local end, one on local reaching its local
/// end from the sandbox.
pub struct PortForwarder {
    phase: watch::Receiver<Phase>,
    /// Nothing where no connection goes through the forwarder: a port in the sandbox reached at
    /// a server's own address.
    failures: Option<Arc<Failures>>,
    #[cfg(test)]
    tasks: Arc<AtomicUsize>,
}

impl PortForwarder {
    /// Forward the port `args` names over `client`'s connection, borrowed or held: the
    /// forwarder, and what forwards, a future that runs on the runtime it is polled in until the
    /// forwarder ends, and stops it when dropped. It is open, or why not, once [`opened`](Self::opened) answers.
    ///
    /// A port in the sandbox over stdio is reached at the loopback address the server answered
    /// for it, its `url`, unless another local address is asked for, whose connections are
    /// carried there; one without its `url` is refused. Over HTTP each connection to the local
    /// address is carried to `/sandboxes/{id}/ports/{port}` in a WebSocket of `virtx.tcp`, in
    /// the project `client` names as it is opened. A port on local is taken as the module
    /// says.
    pub fn open<C>(
        client: C,
        args: PortForwarderArgs,
    ) -> (PortForwarder, impl Future<Output = ()> + Send + use<C>)
    where
        C: Deref<Target = TransportClient> + Send + Sync,
    {
        let PortForwarderArgs {
            sandbox,
            port,
            on,
            url,
            local,
        } = args;
        let way = match on {
            PortSide::Local => Ok(Way::Take {
                target: local.unwrap_or_else(|| format!("127.0.0.1:{port}")),
                failures: Failures::new(format!("port {port} on local")),
            }),
            PortSide::Sandbox => match (reach(&client, &sandbox, port, url.as_deref()), local) {
                (Err(e), _) => Err(e),
                (Ok(Reached::Address(address)), None) => Ok(Way::Served(address)),
                (Ok(reached), local) => {
                    let local = local.as_deref().unwrap_or("127.0.0.1:0");
                    let opening = |e| Error::Client(format!("listening at {local}: {e}"));
                    let listener = std::net::TcpListener::bind(local).map_err(opening);
                    let listener = listener.and_then(|listener| {
                        listener.set_nonblocking(true).map_err(opening)?;
                        Ok((listener.local_addr().map_err(opening)?, listener))
                    });
                    listener.map(|(address, listener)| Way::Listen {
                        reached,
                        listener,
                        address,
                        failures: Failures::new(format!("port {port}, forwarded at {address}")),
                    })
                }
            },
        };
        let (phase, failures) = match &way {
            Err(e) => (Phase::Ended(e.clone()), None),
            Ok(Way::Served(address)) => (Phase::Open(address.to_string()), None),
            Ok(Way::Listen {
                address, failures, ..
            }) => (Phase::Open(address.to_string()), Some(Arc::clone(failures))),
            Ok(Way::Take { failures, .. }) => (Phase::Opening, Some(Arc::clone(failures))),
        };
        let (said, phase) = watch::channel(phase);
        let forwarder = PortForwarder {
            phase,
            failures,
            #[cfg(test)]
            tasks: Arc::default(),
        };
        #[cfg(test)]
        let count = Arc::clone(&forwarder.tasks);
        let forwarding = async move {
            match way {
                Ok(Way::Take { target, failures }) => {
                    let Err(ended) =
                        take_local(&client, &sandbox, port, &target, &failures, &said).await;
                    said.send_replace(Phase::Ended(ended));
                }
                Ok(Way::Listen {
                    reached,
                    listener,
                    failures,
                    ..
                }) => match TcpListener::from_std(listener) {
                    Ok(listener) => {
                        accept(
                            listener,
                            reached,
                            failures,
                            #[cfg(test)]
                            count,
                        )
                        .await
                    }
                    Err(e) => {
                        let e = Error::Client(format!("listening for port {port}: {e}"));
                        said.send_replace(Phase::Ended(e));
                    }
                },
                // Open at the server's own address, which no connection goes through the
                // forwarder to, or ended before it opened: as it is until this is dropped.
                Ok(Way::Served(_)) | Err(_) => std::future::pending().await,
            }
        };
        (forwarder, forwarding)
    }

    /// Answers once it is open with its local end, as `host:port`: for a port in the sandbox
    /// the address it is reached at, for a port on local the target its connections are carried
    /// to; or with why it did not open, such as `404` for a port on local the sandbox was not
    /// made with.
    pub fn opened(&self) -> impl Future<Output = Result<String, Error>> + Send + 'static {
        let mut phase = self.phase.clone();
        async move {
            let seen = phase
                .wait_for(|phase| !matches!(phase, Phase::Opening))
                .await;
            match seen.as_deref() {
                Ok(Phase::Open(local)) => Ok(local.clone()),
                Ok(Phase::Ended(e)) => Err(e.clone()),
                _ => Err(Error::Client("the port forwarder was closed".into())),
            }
        }
    }

    /// Answers once it has ended, with what ended it: for a port on local, `404` once the
    /// sandbox is gone, or what else ended its taking; for a port in the sandbox, which is
    /// forwarded until it is closed, why it did not open. Closed, it answers so.
    pub fn closed(&self) -> impl Future<Output = Error> + Send + 'static {
        let mut phase = self.phase.clone();
        async move {
            let seen = phase
                .wait_for(|phase| matches!(phase, Phase::Ended(_)))
                .await;
            match seen.as_deref() {
                Ok(Phase::Ended(e)) => e.clone(),
                _ => Error::Client("the port forwarder was closed".into()),
            }
        }
    }

    /// Why the last connection through the forwarder that failed did, since one last reached
    /// the port: `None` while none has, and again once a connection reaches it. It is the last
    /// failure, not whether the port is reached now. A server on this machine reaches what it
    /// is itself, with no connection through the forwarder, so for a port in the sandbox at its
    /// own address, and for a port on local over stdio, it has none.
    pub fn last_error(&self) -> Option<ForwardError> {
        let failures = self.failures.as_ref()?;
        failures
            .last
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    #[cfg(test)]
    fn tasks(&self) -> usize {
        self.tasks.load(SeqCst)
    }
}

/// Where the port `port` in the sandbox `sandbox` is reached over `client`: at the address a
/// server on this machine answered for it, its `url`, or in a WebSocket over HTTP.
fn reach(
    client: &TransportClient,
    sandbox: &str,
    port: u16,
    url: Option<&str>,
) -> Result<Reached, Error> {
    match &client.connection {
        Connection::Stdio(_) => match url {
            None => Err(Error::Client(format!(
                "port {port} has no url: a server on this machine answers each port's \
                 address with its sandbox"
            ))),
            Some(url) => served_at(url).map(Reached::Address),
        },
        Connection::Tcp(tcp) => {
            let path = format!("sandboxes/{}/ports/{port}", segment(sandbox));
            let url = (tcp.base.join(&path)).map_err(|e| Error::Client(format!("{path}: {e}")))?;
            Ok(Reached::Tunnel {
                http: tcp.http.clone(),
                credentials: Arc::clone(&tcp.credentials),
                headers: Arc::clone(&tcp.headers),
                project: (client.project.read())
                    .unwrap_or_else(PoisonError::into_inner)
                    .clone(),
                url,
            })
        }
    }
}

/// Where a port in the sandbox is reached: at a server's own address on this machine, or in a
/// WebSocket to `url` that `http` opens with the token and `headers` current then, and
/// `project`.
enum Reached {
    Address(SocketAddr),
    Tunnel {
        http: reqwest::Client,
        credentials: Arc<Credentials>,
        headers: Shared<HeaderMap>,
        project: Option<HeaderValue>,
        url: reqwest::Url,
    },
}

/// How a forwarder reaches its port: a port on local taken, its connections carried to
/// `target`; a port in the sandbox at the address its server answered, or through `listener`, at
/// `address`, its connections carried to where `reached` says.
enum Way {
    Take {
        target: String,
        failures: Arc<Failures>,
    },
    Served(SocketAddr),
    Listen {
        reached: Reached,
        listener: std::net::TcpListener,
        address: SocketAddr,
        failures: Arc<Failures>,
    },
}

/// The address a server on this machine answered for a port, as `http://127.0.0.1:54123`.
fn served_at(url: &str) -> Result<SocketAddr, Error> {
    let parsed = reqwest::Url::parse(url).map_err(|e| Error::Client(format!("{url}: {e}")))?;
    let (Some(host), Some(port)) = (parsed.host_str(), parsed.port_or_known_default()) else {
        return Err(Error::Client(format!("{url} names no host and port")));
    };
    (host, port)
        .to_socket_addrs()
        .map_err(|e| Error::Transport(format!("{url}: {e}")))?
        .next()
        .ok_or_else(|| Error::Transport(format!("{url} is no address")))
}

/// Take `listener`'s connections and carry each to where `reached` says, each failure said to
/// `failures`.
async fn accept(
    listener: TcpListener,
    reached: Reached,
    failures: Arc<Failures>,
    #[cfg(test)] count: Arc<AtomicUsize>,
) {
    let reached = Arc::new(reached);
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                // An error here is a connection's or the process's, and the listener stays open
                // either way.
                let accepted = match accepted {
                    Ok((accepted, _)) => accepted,
                    Err(e) => {
                        if let Some(wait) = accept_backoff(&e) {
                            warning::warn(format!(
                                "{}: accepting a connection failed, trying again in {}s: {e}",
                                failures.port,
                                wait.as_secs()
                            ));
                            tokio::time::sleep(wait).await;
                        }
                        continue;
                    }
                };
                let (reached, failures) = (Arc::clone(&reached), Arc::clone(&failures));
                connections.spawn(async move {
                    // Failed, the connection closes: as a port nothing listens on does.
                    if let Err(failure) = forward(accepted, &reached, &failures).await {
                        failures.fail(failure);
                    }
                });
            }
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
        }
        #[cfg(test)]
        count.store(connections.len(), SeqCst);
    }
}

/// Carry `accepted` to where `reached` says, until either ends.
async fn forward(
    mut accepted: TcpStream,
    reached: &Reached,
    failures: &Failures,
) -> Result<(), ForwardError> {
    match reached {
        Reached::Address(address) => {
            let reaching = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(address));
            let mut reached = match reaching.await {
                Ok(Ok(reached)) => reached,
                Ok(Err(e)) => {
                    return Err(ForwardError::new(format!(
                        "the server's address {address} was not reached: {e}"
                    )));
                }
                Err(_) => {
                    return Err(ForwardError::new(format!(
                        "the server's address {address} was not reached in time"
                    )));
                }
            };
            failures.reached();
            let _ = tokio::io::copy_bidirectional(&mut accepted, &mut reached).await;
            Ok(())
        }
        Reached::Tunnel {
            http,
            credentials,
            headers,
            project,
            url,
        } => {
            let mut headers = current(headers);
            if let Some(project) = project {
                headers.insert(PROJECT_HEADER, project.clone());
            }
            let token = credentials
                .token()
                .await
                .map_err(|e| ForwardError::new(format!("no token to reach the server: {e}")))?;
            match open(http, token.as_deref(), headers, url.clone()).await {
                Ok(socket) => carry(accepted, socket, failures).await,
                Err(refused) => {
                    if refused.status == Some(StatusCode::UNAUTHORIZED.as_u16()) {
                        credentials.refused();
                    }
                    Err(refused)
                }
            }
        }
    }
}

/// The protocol's code `refusal` said, as `not_found` of `not_found: ...`, where it said one.
fn code(refusal: &Error) -> Option<String> {
    let Error::Status(_, said, _) = refusal else {
        return None;
    };
    let (code, _) = said.split_once(": ")?;
    let protocol = !code.is_empty()
        && code
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    protocol.then(|| code.to_owned())
}

/// `error` and each error under it, as `a: b: c`.
fn chain(error: &dyn std::error::Error) -> String {
    let mut said = error.to_string();
    let mut under = error.source();
    while let Some(error) = under {
        said.push_str(&format!(": {error}"));
        under = error.source();
    }
    said
}

/// A WebSocket of `virtx.tcp` to `url`, its handshake an HTTP/1.1 upgrade `http` sends with
/// `token` and `headers`. Refused, why, with the status the server answered if it answered.
async fn open(
    http: &reqwest::Client,
    token: Option<&str>,
    headers: HeaderMap,
    url: reqwest::Url,
) -> Result<WebSocketStream<reqwest::Upgraded>, ForwardError> {
    let key = generate_key();
    let response = authorized(http.get(url), headers, token)
        .header(header::CONNECTION, "Upgrade")
        .header(header::UPGRADE, "websocket")
        .header(header::SEC_WEBSOCKET_VERSION, "13")
        .header(header::SEC_WEBSOCKET_KEY, &key)
        .header(header::SEC_WEBSOCKET_PROTOCOL, TCP_SUBPROTOCOL)
        .send()
        .await
        // Without the URL, which is secret.
        .map_err(|e| {
            ForwardError::new(format!(
                "the server was not reached: {}",
                chain(&e.without_url())
            ))
        })?;
    if response.status() != StatusCode::SWITCHING_PROTOCOLS {
        let response: http::Response<reqwest::Body> = response.into();
        let refusal =
            refused(response.map(|body| -> Body { body.map_err(std::io::Error::other).boxed() }))
                .await;
        return Err(ForwardError {
            message: format!("its WebSocket was refused: {refusal}"),
            source: refusal.source_kind(),
            status: refusal.status(),
            code: code(&refusal),
        });
    }
    let accepted = response
        .headers()
        .get(header::SEC_WEBSOCKET_ACCEPT)
        .is_some_and(|accept| accept.as_bytes() == derive_accept_key(key.as_bytes()).as_bytes());
    if !accepted {
        return Err(ForwardError::new(
            "the server upgraded to a WebSocket without the key it was sent",
        ));
    }
    let upgraded = response.upgrade().await.map_err(|e| {
        ForwardError::new(format!(
            "its WebSocket failed to open: {}",
            chain(&e.without_url())
        ))
    })?;
    Ok(WebSocketStream::from_raw_socket(upgraded, Role::Client, None).await)
}

/// How a carried connection ended.
enum End {
    /// Its TCP ended, by the client.
    Client,
    /// The server closed the WebSocket, saying why if it did.
    Closed(Option<String>),
    /// The WebSocket failed.
    Failed(String),
}

impl End {
    /// The WebSocket's sending failing: closed if the server closed it.
    fn sending(error: tungstenite::Error) -> End {
        match error {
            tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed => {
                End::Closed(None)
            }
            error => End::Failed(error.to_string()),
        }
    }
}

/// Carry bytes between `tcp` and `socket` until either ends: what is read from `tcp` as binary
/// messages, and the binary messages to `tcp`. The port's first byte is said to `failures` as
/// reached. Ended by the server before the port sent a byte, what was seen.
async fn carry(
    mut tcp: TcpStream,
    socket: WebSocketStream<reqwest::Upgraded>,
    failures: &Failures,
) -> Result<(), ForwardError> {
    let (mut sink, mut messages) = socket.split();
    let (mut from, mut to) = tcp.split();
    let mut buffer = vec![0u8; CHUNK];
    let mut answered = false;
    let end = loop {
        tokio::select! {
            read = from.read(&mut buffer) => match read {
                Ok(0) | Err(_) => break End::Client,
                Ok(n) => {
                    if let Err(e) = sink.send(Message::binary(buffer[..n].to_vec())).await {
                        break End::sending(e);
                    }
                }
            },
            received = messages.next() => match received {
                Some(Ok(Message::Binary(bytes))) => {
                    if !answered {
                        answered = true;
                        failures.reached();
                    }
                    if to.write_all(&bytes).await.is_err() {
                        break End::Client;
                    }
                }
                // Answered by the socket itself.
                Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => {}
                // Anything else is the end.
                Some(Ok(Message::Close(frame))) => {
                    break End::Closed(
                        frame.map(|frame| frame.reason.to_string()).filter(|r| !r.is_empty()),
                    );
                }
                Some(Ok(Message::Text(_))) => break End::Failed("it sent text, which no TCP is".into()),
                Some(Err(e)) => break End::sending(e),
                None => break End::Closed(None),
            },
        }
    };
    let _ = sink.send(Message::Close(None)).await;
    if answered {
        return Ok(());
    }
    let before = "before the port sent a byte";
    match end {
        End::Client => Ok(()),
        End::Closed(None) => Err(ForwardError::new(format!(
            "the server closed its WebSocket {before}"
        ))),
        End::Closed(Some(reason)) => Err(ForwardError::new(format!(
            "the server closed its WebSocket {before}: {reason}"
        ))),
        End::Failed(e) => Err(ForwardError::new(format!(
            "its WebSocket failed {before}: {e}"
        ))),
    }
}

/// The longest wait before a WebSocket that failed is opened again.
const MOST_WAIT: Duration = Duration::from_secs(8);

/// Take the port `port` on local of the sandbox `sandbox` over `client`, each connection to it
/// carried to `target`, `host:port`, and each failure on the way said to `failures`: said
/// [open](Phase::Open) to `said` once it is taken, and ended with the error that ends it, `404`
/// for a sandbox or a port that is no one's, or once the sandbox is gone. Once taken, a refusal
/// that can pass, `401`, `429`, a `5xx` or no answer, is taken again after a wait; any other, such
/// as `403`, ends it. Dropped, the port is let go of, and the connections it carries closed.
async fn take_local(
    client: &TransportClient,
    sandbox: &str,
    port: u16,
    target: &str,
    failures: &Arc<Failures>,
    said: &watch::Sender<Phase>,
) -> Result<Infallible, Error> {
    let path = format!("sandboxes/{}/client-ports/{port}", segment(sandbox));
    let Connection::Tcp(tcp) = &client.connection else {
        let request = http::Request::get(format!("/{path}?target={}", segment(target)))
            .body(full(Bytes::new()))
            .map_err(|e| Error::Client(format!("not a request: {e}")))?;
        let response = client.send(request).await?;
        if !response.status().is_success() {
            return Err(refused(response).await);
        }
        said.send_replace(Phase::Open(target.to_owned()));
        let mut held = response.into_body();
        while let Some(frame) = held.frame().await {
            frame.map_err(|e| Error::Transport(format!("port {port} on local: {e}")))?;
        }
        return Err(Error::Status(
            StatusCode::NOT_FOUND.as_u16(),
            format!("not_found: sandbox {sandbox} is gone"),
            Default::default(),
        ));
    };
    let url = (tcp.base.join(&path)).map_err(|e| Error::Client(format!("{path}: {e}")))?;
    let mut carried = JoinSet::new();
    let (mut opened, mut failed) = (false, 0u32);
    loop {
        while carried.try_join_next().is_some() {}
        let mut sent = current(&tcp.headers);
        let project = (client.project.read())
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some(project) = project {
            sent.insert(PROJECT_HEADER, project);
        }
        let waiting = match tcp.credentials.token().await {
            Ok(token) => open(&tcp.http, token.as_deref(), sent, url.clone()).await,
            Err(e) => Err(ForwardError::new(format!(
                "no token to reach the server: {e}"
            ))),
        };
        let mut waiting = match waiting {
            Ok(waiting) => waiting,
            Err(refused) => {
                if refused.status == Some(StatusCode::UNAUTHORIZED.as_u16()) {
                    tcp.credentials.refused();
                }
                // Taken again only past what can pass: a token renewed, a server that is
                // busy or down.
                let passing = match refused.status {
                    None => true,
                    Some(status) => status == 401 || status == 429 || status >= 500,
                };
                if !opened || !passing {
                    let message = format!("port {port} on local: {}", refused.message);
                    return Err(match refused.status {
                        Some(status) => Error::Status(status, message, Default::default()),
                        None => Error::Transport(message),
                    });
                }
                if failed == 0 {
                    failures.fail(ForwardError {
                        message: format!("taking it again: {}", refused.message),
                        ..refused
                    });
                }
                failed += 1;
                let wait = Duration::from_millis(250) * 2u32.pow(failed.min(5));
                tokio::time::sleep(wait.min(MOST_WAIT)).await;
                continue;
            }
        };
        failed = 0;
        if !opened {
            opened = true;
            said.send_replace(Phase::Open(target.to_owned()));
        }
        // Until a connection is said on it, or it ends.
        let connected = loop {
            match waiting.next().await {
                Some(Ok(Message::Binary(bytes))) if bytes.is_empty() => break true,
                Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => {}
                _ => break false,
            }
        };
        if connected {
            carried.spawn(reach_target(
                waiting,
                target.to_owned(),
                Arc::clone(failures),
            ));
        }
    }
}

/// Reach `target` for the connection said on `socket`, and carry the one to the other; or, not
/// reached, close `socket`, which refuses it, and say why to `failures`.
async fn reach_target(
    mut socket: WebSocketStream<reqwest::Upgraded>,
    target: String,
    failures: Arc<Failures>,
) {
    let reached = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(&target)).await;
    let reached = match reached {
        Ok(Ok(reached)) => reached,
        failed => {
            let why = match failed {
                Ok(Err(e)) => e.to_string(),
                _ => "not in time".to_owned(),
            };
            failures.fail(ForwardError::new(format!(
                "{target} was not reached: {why}"
            )));
            let _ = socket.close(None).await;
            return;
        }
    };
    failures.reached();
    let _ = reached.set_nodelay(true);
    if socket.send(Message::binary(Vec::new())).await.is_ok() {
        carry_halves(reached, socket).await;
    }
}

/// Carry bytes between `tcp` and `socket` until both directions have ended: what is read from
/// `tcp` as binary messages, and its end as an empty one; the binary messages to `tcp`, and an
/// empty one as the other end's, `tcp`'s writing shut. A half-close so reaches each end, and an
/// answer to it still comes back. Ended at once by a failure, or a close.
async fn carry_halves(mut tcp: TcpStream, socket: WebSocketStream<reqwest::Upgraded>) {
    let (mut sink, mut messages) = socket.split();
    let (mut from, mut to) = tcp.split();
    let mut buffer = vec![0u8; CHUNK];
    let (mut read_all, mut written_all) = (false, false);
    while !(read_all && written_all) {
        tokio::select! {
            read = from.read(&mut buffer), if !read_all => match read {
                Ok(n) => {
                    read_all = n == 0;
                    if sink.send(Message::binary(buffer[..n].to_vec())).await.is_err() {
                        break;
                    }
                }
                Err(_) => break,
            },
            received = messages.next() => match received {
                Some(Ok(Message::Binary(bytes))) if bytes.is_empty() => {
                    written_all = true;
                    let _ = to.shutdown().await;
                }
                Some(Ok(Message::Binary(bytes))) => {
                    if to.write_all(&bytes).await.is_err() {
                        break;
                    }
                }
                // Answered by the socket itself.
                Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => {}
                _ => break,
            },
        }
    }
    let _ = sink.send(Message::Close(None)).await;
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::Auth;
    use axum::{
        extract::{
            Path,
            ws::{self, WebSocketUpgrade},
        },
        response::IntoResponse,
    };

    /// A server under the base path `/virtx/`, whose port `open` echoes what it is sent, whose
    /// port `deaf` closes its WebSocket at once, `gone` closes it saying why, `late` closes its
    /// first connection at once and echoes every later one, and whose every other port is
    /// refused. Given `token`, it wants it as a bearer token.
    async fn serve(token: Option<&'static str>) -> String {
        let late = Arc::new(AtomicUsize::new(0));
        let app = axum::Router::new().route(
            "/virtx/sandboxes/{sandbox}/ports/{port}",
            axum::routing::get(
                move |Path((sandbox, _port)): Path<(String, u16)>,
                      headers: HeaderMap,
                      upgrade: WebSocketUpgrade| async move {
                    let first = sandbox == "late" && late.fetch_add(1, SeqCst) == 0;
                    let offered = headers
                        .get(header::SEC_WEBSOCKET_PROTOCOL)
                        .is_some_and(|value| value == TCP_SUBPROTOCOL);
                    if !["open", "deaf", "gone", "late"].contains(&sandbox.as_str()) || !offered {
                        return (
                            StatusCode::NOT_FOUND,
                            axum::Json(serde_json::json!({
                                "error": {"code": "not_found", "message": "no such port"}
                            })),
                        )
                            .into_response();
                    }
                    if let Some(token) = token
                        && headers.get(header::AUTHORIZATION).map(|v| v.as_bytes())
                            != Some(format!("Bearer {token}").as_bytes())
                    {
                        return StatusCode::UNAUTHORIZED.into_response();
                    }
                    if sandbox == "deaf" || first {
                        // Closed at once, as a port nothing listens on is.
                        return upgrade.protocols([TCP_SUBPROTOCOL]).on_upgrade(
                            |mut socket| async move {
                                let _ = socket.send(ws::Message::Close(None)).await;
                            },
                        );
                    }
                    if sandbox == "gone" {
                        return upgrade.protocols([TCP_SUBPROTOCOL]).on_upgrade(
                            |mut socket| async move {
                                let frame = ws::CloseFrame {
                                    code: 1011,
                                    reason: "no machine runs to reach port 7000 in".into(),
                                };
                                let _ = socket.send(ws::Message::Close(Some(frame))).await;
                            },
                        );
                    }
                    upgrade
                        .protocols([TCP_SUBPROTOCOL])
                        .on_upgrade(|mut socket| async move {
                            while let Some(Ok(message)) = socket.recv().await {
                                if let ws::Message::Binary(_) = message
                                    && socket.send(message).await.is_err()
                                {
                                    break;
                                }
                            }
                        })
                },
            ),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/virtx/", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });
        base
    }

    #[tokio::test]
    async fn each_connection_is_carried_in_a_websocket_to_the_url_under_the_base() {
        let client =
            TransportClient::connect(&serve(None).await, Auth::None, HeaderMap::new()).unwrap();
        let client = Arc::new(client);
        let (forward, forwarding) =
            PortForwarder::open(Arc::clone(&client), PortForwarderArgs::new("open", 7000));
        let forwarding = tokio::spawn(forwarding);
        let address: SocketAddr = forward.opened().await.unwrap().parse().unwrap();
        assert!(address.ip().is_loopback());

        // Two at once, each its own WebSocket.
        let mut first = TcpStream::connect(address).await.unwrap();
        let mut second = TcpStream::connect(address).await.unwrap();
        for (stream, said) in [(&mut first, b"one"), (&mut second, b"two")] {
            stream.write_all(said).await.unwrap();
            let mut echoed = [0u8; 3];
            stream.read_exact(&mut echoed).await.unwrap();
            assert_eq!(&echoed, said);
        }
        assert_eq!(forward.last_error(), None, "each reached the port");

        // Gone with the forward: looked for a minute, ending once it refuses.
        forwarding.abort();
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        while TcpStream::connect(address).await.is_ok() && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(TcpStream::connect(address).await.is_err());
    }

    /// The forwarder's last error, looked for a minute: it is kept once the connection that
    /// failed has closed, not before.
    async fn failed(forward: &PortForwarder) -> ForwardError {
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        loop {
            if let Some(failed) = forward.last_error() {
                return failed;
            }
            assert!(std::time::Instant::now() < deadline, "no error was kept");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn a_connection_the_server_refuses_is_closed_and_says_why() {
        let client =
            TransportClient::connect(&serve(None).await, Auth::None, HeaderMap::new()).unwrap();
        let client = Arc::new(client);
        let (forward, forwarding) =
            PortForwarder::open(Arc::clone(&client), PortForwarderArgs::new("shut", 7000));
        let _forwarding = tokio::spawn(forwarding);
        let address: SocketAddr = forward.opened().await.unwrap().parse().unwrap();
        assert_eq!(forward.last_error(), None);
        let mut stream = TcpStream::connect(address).await.unwrap();
        let mut read = Vec::new();
        assert_eq!(stream.read_to_end(&mut read).await.unwrap(), 0);

        let failed = failed(&forward).await;
        assert_eq!(failed.status, Some(404));
        assert_eq!(failed.code.as_deref(), Some("not_found"));
        let named = format!("port 7000, forwarded at {}: ", address);
        assert!(failed.message.starts_with(&named), "{}", failed.message);
        assert!(
            failed.message.contains("no such port"),
            "{}",
            failed.message
        );
    }

    #[tokio::test]
    async fn a_websocket_closed_before_the_port_sent_a_byte_says_so() {
        let client =
            TransportClient::connect(&serve(None).await, Auth::None, HeaderMap::new()).unwrap();
        let client = Arc::new(client);
        let (forward, forwarding) =
            PortForwarder::open(Arc::clone(&client), PortForwarderArgs::new("deaf", 7000));
        let _forwarding = tokio::spawn(forwarding);
        let address: SocketAddr = forward.opened().await.unwrap().parse().unwrap();
        let mut stream = TcpStream::connect(address).await.unwrap();
        // Sent or not before the server closes it, the line goes nowhere.
        let _ = stream.write_all(b"hello\n").await;
        let mut read = Vec::new();
        let _ = stream.read_to_end(&mut read).await;
        assert!(read.is_empty());

        let failed = failed(&forward).await;
        assert_eq!(failed.status, None);
        assert!(
            failed.message.starts_with("port 7000, forwarded at "),
            "{}",
            failed.message
        );
        assert!(
            failed
                .message
                .ends_with("the server closed its WebSocket before the port sent a byte"),
            "{}",
            failed.message
        );
    }

    #[tokio::test]
    async fn a_websocket_closed_with_a_reason_says_the_servers_reason() {
        let client =
            TransportClient::connect(&serve(None).await, Auth::None, HeaderMap::new()).unwrap();
        let client = Arc::new(client);
        let (forward, forwarding) =
            PortForwarder::open(Arc::clone(&client), PortForwarderArgs::new("gone", 7000));
        let _forwarding = tokio::spawn(forwarding);
        let address: SocketAddr = forward.opened().await.unwrap().parse().unwrap();
        let mut stream = TcpStream::connect(address).await.unwrap();
        let _ = stream.read_to_end(&mut Vec::new()).await;
        let failed = failed(&forward).await;
        assert!(
            failed.message.ends_with(
                "the server closed its WebSocket before the port sent a byte: no machine runs \
                 to reach port 7000 in"
            ),
            "{}",
            failed.message
        );
    }

    #[tokio::test]
    async fn a_connection_that_reaches_the_port_clears_the_last_error() {
        let client =
            TransportClient::connect(&serve(None).await, Auth::None, HeaderMap::new()).unwrap();
        let client = Arc::new(client);
        let (forward, forwarding) =
            PortForwarder::open(Arc::clone(&client), PortForwarderArgs::new("late", 7000));
        let _forwarding = tokio::spawn(forwarding);
        let address: SocketAddr = forward.opened().await.unwrap().parse().unwrap();
        let mut stream = TcpStream::connect(address).await.unwrap();
        let _ = stream.read_to_end(&mut Vec::new()).await;
        failed(&forward).await;

        let mut stream = TcpStream::connect(address).await.unwrap();
        stream.write_all(b"up").await.unwrap();
        let mut echoed = [0u8; 2];
        stream.read_exact(&mut echoed).await.unwrap();
        assert_eq!(&echoed, b"up");
        assert_eq!(forward.last_error(), None, "the port was reached");
    }

    #[test]
    fn an_accept_error_of_one_connection_is_taken_again_at_once_and_the_processs_after_a_wait() {
        use std::io::{Error, ErrorKind};
        assert_eq!(
            accept_backoff(&Error::from(ErrorKind::ConnectionAborted)),
            None
        );
        assert_eq!(
            accept_backoff(&Error::from(ErrorKind::ConnectionReset)),
            None
        );
        // EMFILE, as the process running out of descriptors answers.
        assert_eq!(
            accept_backoff(&Error::from_raw_os_error(24)),
            Some(Duration::from_secs(1))
        );
    }

    /// Over stdio the server on this machine answers each port's address, and a port without
    /// it is refused.
    #[tokio::test]
    async fn a_port_over_stdio_without_its_address_is_refused() {
        let (ours, _theirs) = tokio::io::duplex(64);
        let client = TransportClient::over(ours);
        let (forward, _forwarding) =
            PortForwarder::open(&client, PortForwarderArgs::new("s", 5173));
        let refused = forward.opened().await.unwrap_err();
        assert!(
            refused.to_string().contains("port 5173 has no url"),
            "{refused}"
        );
    }

    #[tokio::test]
    async fn a_connection_goes_with_the_auth_current_when_it_is_made() {
        let client =
            TransportClient::connect(&serve(Some("later")).await, Auth::None, HeaderMap::new())
                .unwrap();
        let client = Arc::new(client);
        let (forward, forwarding) =
            PortForwarder::open(Arc::clone(&client), PortForwarderArgs::new("open", 7000));
        let _forwarding = tokio::spawn(forwarding);
        let address: SocketAddr = forward.opened().await.unwrap().parse().unwrap();
        let mut stream = TcpStream::connect(address).await.unwrap();
        let mut read = Vec::new();
        assert_eq!(stream.read_to_end(&mut read).await.unwrap(), 0, "refused");
        assert_eq!(failed(&forward).await.status, Some(401));

        client
            .set_auth(Auth::Bearer {
                token: "later".into(),
            })
            .unwrap();
        let mut stream = TcpStream::connect(address).await.unwrap();
        stream.write_all(b"now").await.unwrap();
        let mut echoed = [0u8; 3];
        stream.read_exact(&mut echoed).await.unwrap();
        assert_eq!(&echoed, b"now");
    }

    /// Over stdio, a port asked for at another local address is carried to the server's own.
    #[tokio::test]
    async fn a_port_over_stdio_at_another_address_is_carried_to_the_servers() {
        let served = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", served.local_addr().unwrap());
        let (ours, _theirs) = tokio::io::duplex(64);
        let client = Arc::new(TransportClient::over(ours));
        let free = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let local = free.local_addr().unwrap().to_string();
        drop(free);
        let args = PortForwarderArgs::new("s", 5173).url(url).local(&local);
        let (forward, forwarding) = PortForwarder::open(client, args);
        let _forwarding = tokio::spawn(forwarding);
        assert_eq!(forward.opened().await.unwrap(), local);

        let mut stream = TcpStream::connect(&local).await.unwrap();
        let (mut reached, _) = served.accept().await.unwrap();
        stream.write_all(b"up").await.unwrap();
        let mut said = [0u8; 2];
        reached.read_exact(&mut said).await.unwrap();
        assert_eq!(&said, b"up");
        assert_eq!(forward.last_error(), None);
    }

    #[tokio::test]
    async fn a_loopback_address_is_reached_as_it_is() {
        let address = served_at("http://127.0.0.1:54123").unwrap();
        assert_eq!(address, "127.0.0.1:54123".parse().unwrap());
        assert!(served_at("ports/abc").is_err());
    }

    #[tokio::test]
    async fn completed_connection_tasks_are_reaped() {
        let client =
            TransportClient::connect(&serve(None).await, Auth::None, HeaderMap::new()).unwrap();
        let client = Arc::new(client);
        let (forward, forwarding) =
            PortForwarder::open(Arc::clone(&client), PortForwarderArgs::new("open", 7000));
        let _forwarding = tokio::spawn(forwarding);
        let address: SocketAddr = forward.opened().await.unwrap().parse().unwrap();

        for i in 0..10 {
            let mut stream = TcpStream::connect(address).await.unwrap();
            let msg = format!("msg{i}");
            stream.write_all(msg.as_bytes()).await.unwrap();
            let mut echoed = vec![0u8; msg.len()];
            stream.read_exact(&mut echoed).await.unwrap();
            assert_eq!(&echoed, msg.as_bytes());
            drop(stream);
        }

        // Looked for a minute: what is tried is that they go, not how soon.
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        while forward.tasks() > 0 && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(forward.tasks(), 0, "completed tasks were not reaped");
    }

    #[tokio::test]
    async fn refused_connection_tasks_are_reaped() {
        let client =
            TransportClient::connect(&serve(None).await, Auth::None, HeaderMap::new()).unwrap();
        let client = Arc::new(client);
        let (forward, forwarding) =
            PortForwarder::open(Arc::clone(&client), PortForwarderArgs::new("shut", 7000));
        let _forwarding = tokio::spawn(forwarding);
        let address: SocketAddr = forward.opened().await.unwrap().parse().unwrap();

        for _ in 0..5 {
            let mut stream = TcpStream::connect(address).await.unwrap();
            let mut read = Vec::new();
            assert_eq!(stream.read_to_end(&mut read).await.unwrap(), 0);
        }

        // Looked for a minute: what is tried is that they go, not how soon.
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        while forward.tasks() > 0 && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(forward.tasks(), 0, "failed tasks were not reaped");
    }

    /// A port on local, taken over HTTP.
    #[cfg(unix)]
    mod local {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering::SeqCst},
        };

        use axum::{
            Router,
            extract::{
                Path,
                ws::{Message as WsMessage, WebSocketUpgrade},
            },
            response::IntoResponse,
            routing::get,
        };
        use tokio::sync::mpsc;

        use super::*;

        /// A server over HTTP whose sandbox `s` has the port 9222 on local: each WebSocket to it is
        /// said one connection at once, and once that is answered it is sent `hi`, and what comes
        /// back is sent to `answered`. Every WebSocket opened is counted in `opened`; another port, or
        /// another sandbox, is `404`.
        async fn serve(
            opened: Arc<AtomicUsize>,
            answered: mpsc::UnboundedSender<Vec<u8>>,
        ) -> String {
            let app = Router::new().route(
                "/sandboxes/{id}/client-ports/{port}",
                get(
                    move |Path((id, port)): Path<(String, u16)>, upgrade: WebSocketUpgrade| {
                        let (opened, answered) = (Arc::clone(&opened), answered.clone());
                        async move {
                            if id != "s" || port != 9222 {
                                let body =
                                    r#"{"error": {"code": "not_found", "message": "no such port"}}"#;
                                return (StatusCode::NOT_FOUND, body).into_response();
                            }
                            opened.fetch_add(1, SeqCst);
                            upgrade
                                .protocols(["virtx.tcp"])
                                .on_upgrade(move |mut socket| async move {
                                    let said = WsMessage::Binary(Vec::new().into());
                                    if socket.send(said).await.is_err() {
                                        return;
                                    }
                                    match socket.recv().await {
                                        Some(Ok(WsMessage::Binary(bytes))) if bytes.is_empty() => {}
                                        _ => return,
                                    }
                                    let hi = WsMessage::Binary(b"hi".to_vec().into());
                                    let _ = socket.send(hi).await;
                                    if let Some(Ok(WsMessage::Binary(bytes))) = socket.recv().await {
                                        let _ = answered.send(bytes.to_vec());
                                    }
                                })
                        }
                    },
                ),
            );
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}/", listener.local_addr().unwrap());
            tokio::spawn(async move { axum::serve(listener, app).await });
            base
        }

        /// Each connection said is carried between its WebSocket and the target, and another
        /// WebSocket waits for the next; the forwarder answers the target as its local end.
        #[tokio::test]
        async fn each_connection_said_is_carried_to_the_target() {
            let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let at = target.local_addr().unwrap().to_string();
            let (answered, mut answers) = mpsc::unbounded_channel();
            let opened = Arc::new(AtomicUsize::new(0));
            let base = serve(Arc::clone(&opened), answered).await;
            let client = TransportClient::connect(&base, Auth::None, HeaderMap::new()).unwrap();
            let args = PortForwarderArgs::new("s", 9222)
                .on(PortSide::Local)
                .local(&at);
            let (forward, forwarding) = PortForwarder::open(&client, args);
            let carried = async {
                assert_eq!(forward.opened().await.unwrap(), at);
                let (mut reached, _) = target.accept().await.unwrap();
                let mut said = [0u8; 2];
                reached.read_exact(&mut said).await.unwrap();
                assert_eq!(&said, b"hi");
                reached.write_all(b"back").await.unwrap();
                answers.recv().await.unwrap()
            };
            tokio::select! {
                answer = carried => assert_eq!(answer, b"back"),
                () = forwarding => panic!("ended"),
            }
            assert!(
                opened.load(SeqCst) >= 2,
                "no other WebSocket waits for the next"
            );
            assert_eq!(forward.last_error(), None);
        }

        /// A connection whose target is not reached has its WebSocket closed, which refuses it,
        /// and is the forwarder's last error.
        #[tokio::test]
        async fn a_connection_whose_target_is_not_reached_is_refused_and_says_why() {
            let unreached = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let at = unreached.local_addr().unwrap().to_string();
            drop(unreached);
            let (answered, mut answers) = mpsc::unbounded_channel();
            let base = serve(Arc::default(), answered).await;
            let client = TransportClient::connect(&base, Auth::None, HeaderMap::new()).unwrap();
            let args = PortForwarderArgs::new("s", 9222)
                .on(PortSide::Local)
                .local(&at);
            let (forward, forwarding) = PortForwarder::open(&client, args);
            let failed = async {
                // Looked for a minute.
                let deadline = std::time::Instant::now() + Duration::from_secs(60);
                loop {
                    if let Some(failed) = forward.last_error() {
                        return failed;
                    }
                    assert!(std::time::Instant::now() < deadline, "no error was kept");
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            };
            let failed = tokio::select! {
                failed = failed => failed,
                () = forwarding => panic!("ended"),
            };
            let named = format!("port 9222 on local: {at} was not reached: ");
            assert!(failed.message.starts_with(&named), "{}", failed.message);
            assert!(
                answers.try_recv().is_err(),
                "a connection not reached was answered"
            );
        }

        /// A port that is no one's is why it does not open, and what ended it.
        #[tokio::test]
        async fn a_port_the_server_has_not_is_refused() {
            let (answered, _answers) = mpsc::unbounded_channel();
            let base = serve(Arc::default(), answered).await;
            let client = TransportClient::connect(&base, Auth::None, HeaderMap::new()).unwrap();
            let args = PortForwarderArgs::new("s", 5037).on(PortSide::Local);
            let (forward, forwarding) = PortForwarder::open(&client, args);
            forwarding.await;
            assert_eq!(forward.opened().await.unwrap_err().status(), Some(404));
            assert_eq!(forward.closed().await.status(), Some(404));
        }

        /// Refused once it was taken, with what will not pass, such as a member removed, it ends
        /// with that refusal rather than taking it again.
        #[tokio::test]
        async fn a_refusal_after_it_was_taken_ends_it() {
            let taken = Arc::new(AtomicUsize::new(0));
            let counted = Arc::clone(&taken);
            let app = Router::new().route(
                "/sandboxes/{id}/client-ports/{port}",
                get(move |upgrade: WebSocketUpgrade| {
                    let taken = Arc::clone(&counted);
                    async move {
                        if taken.fetch_add(1, SeqCst) > 0 {
                            let body =
                                r#"{"error": {"code": "forbidden", "message": "not a member"}}"#;
                            return (StatusCode::FORBIDDEN, body).into_response();
                        }
                        // Taken, then closed with no connection said.
                        upgrade
                            .protocols(["virtx.tcp"])
                            .on_upgrade(|socket| async move { drop(socket) })
                    }
                }),
            );
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}/", listener.local_addr().unwrap());
            tokio::spawn(async move { axum::serve(listener, app).await });
            let client = TransportClient::connect(&base, Auth::None, HeaderMap::new()).unwrap();
            let args = PortForwarderArgs::new("s", 9222)
                .on(PortSide::Local)
                .local("127.0.0.1:9");
            let (forward, forwarding) = PortForwarder::open(&client, args);
            tokio::time::timeout(Duration::from_secs(60), forwarding)
                .await
                .expect("taken again");
            assert_eq!(taken.load(SeqCst), 2, "refused only once it was taken");
            assert_eq!(forward.closed().await.status(), Some(403));
        }
    }
}

#[cfg(feature = "ffi")]
mod ffi {
    //! [`PortForwarder`] behind the C ABI: opened over a client, forwarding on its runtime, and
    //! freed.

    use std::{
        ffi::c_int,
        ops::Deref,
        ptr,
        sync::{Arc, Mutex},
    };

    use serde::Serialize;
    use tokio::{
        runtime::Handle,
        task::{AbortHandle, JoinSet},
    };

    use super::{PortForwarder, PortForwarderArgs};
    use crate::{
        Client,
        client::ffi::VirtxClient,
        ffi::{VIRTX_ERROR, VIRTX_OK, VirtxBytes, VirtxFuture, failure, json, start, write},
        transport::TransportClient,
    };

    /// A client held by what forwards, for as long as it forwards.
    struct Held(Arc<Client>);

    impl Deref for Held {
        type Target = TransportClient;

        fn deref(&self) -> &TransportClient {
            &self.0.inner
        }
    }

    /// Where a forwarder is, as `host:port`.
    #[derive(Serialize)]
    struct Opened {
        local: String,
    }

    /// A `PortForwarder`, as C holds it: forwarding until it is freed, and the runtime its
    /// requests run on.
    pub struct VirtxPortForwarder {
        forwarder: PortForwarder,
        forwarding: AbortHandle,
        runtime: Handle,
        requests: Mutex<JoinSet<()>>,
    }

    impl Drop for VirtxPortForwarder {
        fn drop(&mut self) {
            self.forwarding.abort();
        }
    }

    /// Forward a port of a sandbox's, on either side of it, given
    /// `{"sandbox": "...", "port": n, "on"?: "sandbox" | "local", "url"?: "...",
    /// "local"?: "host:port"}`: write the forwarder to `forwarder`, and hand C the future that
    /// answers once it is open, as `{"local": "host:port"}`, or why it did not open.
    ///
    /// A port in the sandbox is reached at `local`, a free port on `127.0.0.1` without it; over
    /// stdio, at the port's `url`, as the server answered it, without it, and one without its
    /// `url` is refused. A port on local, one the sandbox was made with `"on": "local"`, is
    /// reached by its processes at `10.0.2.2:{port}`, each connection carried to `local` on this
    /// machine, `127.0.0.1:{port}` without it, and refused when that is not reached; its opening
    /// is refused `404` for a port the sandbox was not made with. What fails on the way is
    /// handed to the function `virtx_set_warn` set, as well as kept for
    /// `virtx_port_forwarder_last_error`.
    ///
    /// It runs on `client`'s runtime, so the client's last release stops it, and is freed with
    /// `virtx_port_forwarder_free`; arguments that are not these write null to `forwarder`.
    ///
    /// # Safety
    ///
    /// `client` is one `virtx_client_spawn` or `virtx_client_connect` wrote, still held, and
    /// `args` as in `virtx_client_build_image`. `forwarder` is writable.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_port_forwarder_open(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
        forwarder: *mut *mut VirtxPortForwarder,
    ) -> *mut VirtxFuture {
        let held = unsafe { &(*client).0 };
        let args = match unsafe { json::<PortForwarderArgs>(args, args_len) } {
            Ok(args) => args,
            Err(e) => {
                unsafe { *forwarder = ptr::null_mut() };
                return held.future(|_| async move { Err::<(), _>(e) });
            }
        };
        let mut opened = None;
        let forwarding = held.spawn(|client| {
            let (made, forwarding) = PortForwarder::open(Held(client), args);
            opened = Some(made);
            forwarding
        });
        let opened = opened.expect("made as it was spawned");
        let opening = opened.opened();
        let made = VirtxPortForwarder {
            forwarder: opened,
            forwarding,
            runtime: held.handle(),
            requests: Mutex::default(),
        };
        unsafe { *forwarder = Box::into_raw(Box::new(made)) };
        held.future(|_| async move { opening.await.map(|local| Opened { local }) })
    }

    /// Hand C the future that answers once `forwarder` has ended, with what ended it as an
    /// error: for a port on local, `404` once its sandbox is gone. A port in the sandbox is
    /// forwarded until it is freed, which answers the future as the call ended.
    ///
    /// # Safety
    ///
    /// `forwarder` is one `virtx_port_forwarder_open` wrote, not yet freed.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_port_forwarder_closed(
        forwarder: *const VirtxPortForwarder,
    ) -> *mut VirtxFuture {
        let held = unsafe { &*forwarder };
        let closed = held.forwarder.closed();
        start(&held.runtime, &held.requests, async move {
            failure(closed.await)
        })
    }

    /// Why the last connection through `forwarder` that failed did, since one last reached the
    /// port: its WebSocket refused, or closed before the port sent a byte, with the server's
    /// reason if it gave one, or its target not reached. `VIRTX_OK` with `null` written to `out`
    /// while none has, or `VIRTX_ERROR` with the failure written as a call's error is, its
    /// message naming the port: `{"message", "status"?, "code"?}`, `status` and `code` the
    /// server's refusal of the WebSocket. Each such failure is also handed to the function
    /// `virtx_set_warn` set.
    ///
    /// # Safety
    ///
    /// `forwarder` is one `virtx_port_forwarder_open` wrote, not yet freed. `out` is writable.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_port_forwarder_last_error(
        forwarder: *const VirtxPortForwarder,
        out: *mut VirtxBytes,
    ) -> c_int {
        let (status, bytes) = match unsafe { &(*forwarder).forwarder }.last_error() {
            None => (VIRTX_OK, b"null".to_vec()),
            Some(failed) => (
                VIRTX_ERROR,
                serde_json::to_vec(&failed).expect("a failure encodes"),
            ),
        };
        unsafe { write(out, bytes) };
        status
    }

    /// Stop `forwarder` and free it, answering its futures not yet answered as ended. Null does
    /// nothing.
    ///
    /// # Safety
    ///
    /// `forwarder` is null, or one `virtx_port_forwarder_open` wrote, not used again.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_port_forwarder_free(forwarder: *mut VirtxPortForwarder) {
        if !forwarder.is_null() {
            drop(unsafe { Box::from_raw(forwarder) });
        }
    }

    #[cfg(test)]
    mod tests {
        use std::ptr;

        use super::*;
        use crate::{
            client::ffi::{virtx_client_connect, virtx_client_release},
            ffi::virtx_bytes_free,
        };

        /// What the library warned of, through the hook this module's test sets.
        static WARNED: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

        #[test]
        fn a_forwarder_answers_its_loopback_address_until_it_is_freed() {
            let mut client = ptr::null_mut();
            let mut out = VirtxBytes {
                data: ptr::null_mut(),
                len: 0,
            };
            // Nothing listens there: each connection to the forwarder is closed.
            let url = c"http://127.0.0.1:9/";
            let status = unsafe {
                virtx_client_connect(
                    url.as_ptr(),
                    std::ptr::null(),
                    0,
                    std::ptr::null(),
                    0,
                    &mut client,
                    &mut out,
                )
            };
            assert_eq!(status, crate::ffi::VIRTX_OK);

            unsafe extern "C" fn warned(message: *const std::ffi::c_char) {
                let message = unsafe { std::ffi::CStr::from_ptr(message) };
                WARNED
                    .lock()
                    .unwrap()
                    .push(message.to_string_lossy().into_owned());
            }
            crate::warning::ffi::virtx_set_warn(Some(warned));

            let args = br#"{"sandbox": "abc", "port": 7009}"#;
            let mut forwarder = ptr::null_mut();
            let opening = unsafe {
                virtx_port_forwarder_open(client, args.as_ptr(), args.len(), &mut forwarder)
            };
            assert_eq!(
                unsafe { crate::ffi::virtx_future_wait(opening, &mut out) },
                crate::ffi::VIRTX_OK
            );
            unsafe { crate::ffi::virtx_future_free(opening) };
            let written = unsafe { std::slice::from_raw_parts(out.data, out.len) };
            let opened: serde_json::Value = serde_json::from_slice(written).unwrap();
            let address: std::net::SocketAddr = opened["local"].as_str().unwrap().parse().unwrap();
            assert!(address.ip().is_loopback());
            unsafe { virtx_bytes_free(out) };

            let mut stream = std::net::TcpStream::connect(address).unwrap();
            let mut read = Vec::new();
            assert_eq!(
                std::io::Read::read_to_end(&mut stream, &mut read).unwrap(),
                0
            );

            // Why, kept and warned of, each naming the port: looked for a minute.
            let named = format!("port 7009, forwarded at {address}: the server was not reached");
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
            let failed = loop {
                let mut out = VirtxBytes {
                    data: ptr::null_mut(),
                    len: 0,
                };
                let status = unsafe { virtx_port_forwarder_last_error(forwarder, &mut out) };
                let written = unsafe { std::slice::from_raw_parts(out.data, out.len) };
                let failed: serde_json::Value = serde_json::from_slice(written).unwrap();
                unsafe { virtx_bytes_free(out) };
                if status == VIRTX_ERROR {
                    break failed;
                }
                assert_eq!((status, &failed), (VIRTX_OK, &serde_json::Value::Null));
                assert!(std::time::Instant::now() < deadline, "no error was kept");
                std::thread::sleep(std::time::Duration::from_millis(10));
            };
            let message = failed["message"].as_str().unwrap();
            assert!(message.starts_with(&named), "{message}");
            assert_eq!(failed.get("status"), None);
            let warned = || WARNED.lock().unwrap().iter().any(|m| m == message);
            while !warned() {
                assert!(std::time::Instant::now() < deadline, "no warning was given");
                std::thread::sleep(std::time::Duration::from_millis(10));
            }

            unsafe { virtx_port_forwarder_free(forwarder) };
            unsafe { virtx_client_release(client) };
            // A process another test spawns meanwhile holds the port's descriptor until it
            // execs, and the port answers until then.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while std::net::TcpStream::connect(address).is_ok() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "the port still answers after the forwarder is freed"
                );
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
    }
}
