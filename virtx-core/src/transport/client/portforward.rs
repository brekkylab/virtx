//! A sandbox's port, forwarded to this machine's loopback: what making the sandbox answered for
//! it, as an address any TCP client connects to.
//!
//! A server on this machine answers a loopback address already, which is reached as it is. A
//! server over HTTP answers a URL relative to its base, which only a WebSocket of the
//! subprotocol `virtx.tcp` opens: so each connection to a loopback port of this client's own
//! is carried in one, its bytes as binary messages, opened by the same HTTP client the requests
//! go by.
//!
//! A connection that reaches nothing in the sandbox -- its WebSocket refused, or closed before
//! the port sent a byte, as it is when nothing listens there -- is closed as the server closed
//! it, and why is kept as the forwarder's [last error](PortForwarder::last_error) and warned
//! of, naming the port and never its URL, which is secret.

#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use std::{
    net::{Ipv4Addr, SocketAddr},
    sync::{Arc, Mutex, PoisonError},
};

use super::{
    Body, Connection, TransportClient,
    credentials::Credentials,
    tcp::{Shared, authorized, current},
};
use crate::{Error, transport::http::refused, warning};
use futures_util::{SinkExt, StreamExt};
use http::{HeaderMap, StatusCode, header};
use http_body_util::BodyExt;
use serde::Serialize;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
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

/// Why a connection to a forwarder reached nothing in the sandbox: its WebSocket refused, the
/// server not reached, or the WebSocket closed before the port sent a byte, as the server
/// closes it when nothing listens on the port.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ForwardError {
    /// For a person: the port, and what the server answered.
    pub message: String,
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
            status: None,
            code: None,
        }
    }
}

/// A forwarder's failures: the port they are of, as a person knows it, and the last.
struct Failures {
    port: String,
    last: Mutex<Option<ForwardError>>,
}

impl Failures {
    /// Keep `failed`, named for the port, as the last, and warn of it.
    fn fail(&self, failed: ForwardError) {
        let failed = ForwardError {
            message: format!("{}: {}", self.port, failed.message),
            ..failed
        };
        warning::warn(failed.message.clone());
        *self.last.lock().unwrap_or_else(PoisonError::into_inner) = Some(failed);
    }
}

/// A sandbox's port at an address on this machine's loopback, for as long as this is held. It
/// runs on the runtime it was opened in, and stops with it.
pub struct PortForwarder {
    address: SocketAddr,
    /// What takes the address's connections and carries each, ended when this drops; nothing
    /// where the address is the server's own.
    _forwarding: Option<JoinSet<()>>,
    /// Nothing where the address is the server's own, which no connection goes through.
    failures: Option<Arc<Failures>>,
    #[cfg(test)]
    tasks: Arc<AtomicUsize>,
}

impl PortForwarder {
    /// The port making a sandbox answered `url` for, at an address on this machine's loopback,
    /// reached over `client`'s connection. `port`, its number in the sandbox where the caller
    /// knows it, names it in what a connection that fails says.
    ///
    /// Over stdio the server is on this machine, and its loopback address is the forwarder's.
    /// Over HTTP `url` is resolved against the server's base, and each connection to a loopback
    /// port of the forwarder's own is carried to it in a WebSocket of `virtx.tcp`.
    pub async fn open(
        client: &TransportClient,
        url: &str,
        port: Option<u16>,
    ) -> Result<PortForwarder, Error> {
        match &client.connection {
            Connection::Stdio(_) => PortForwarder::direct(url).await,
            Connection::Tcp(tcp) => match tcp.base.join(url) {
                Ok(url) => {
                    PortForwarder::tunnel(
                        tcp.http.clone(),
                        Arc::clone(&tcp.credentials),
                        Arc::clone(&tcp.headers),
                        url,
                        port,
                    )
                    .await
                }
                // Its message would quote the URL, which is secret.
                Err(e) => Err(format!("a port's URL that does not read as one: {e}")),
            },
        }
        .map_err(Error::Transport)
    }

    /// Where the port is reached: on this machine's loopback.
    pub fn address(&self) -> SocketAddr {
        self.address
    }

    /// Why the last connection to the forwarder that reached nothing in the sandbox failed;
    /// `None` while none has. A server on this machine is reached at its own address, with no
    /// connection through the forwarder, so it has none.
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

    /// The loopback address a server on this machine answered, as `http://127.0.0.1:54123`.
    async fn direct(url: &str) -> Result<PortForwarder, String> {
        let parsed = reqwest::Url::parse(url).map_err(|e| format!("{url}: {e}"))?;
        let (Some(host), Some(port)) = (parsed.host_str(), parsed.port_or_known_default()) else {
            return Err(format!("{url} names no host and port"));
        };
        let address = tokio::net::lookup_host((host, port))
            .await
            .map_err(|e| format!("{url}: {e}"))?
            .next()
            .ok_or_else(|| format!("{url} is no address"))?;
        Ok(PortForwarder {
            address,
            _forwarding: None,
            failures: None,
            #[cfg(test)]
            tasks: Arc::default(),
        })
    }

    /// A loopback port of this client's own, each connection to it carried to `url` in a
    /// WebSocket that `http` opens with the token and `headers` current then.
    async fn tunnel(
        http: reqwest::Client,
        credentials: Arc<Credentials>,
        headers: Shared<HeaderMap>,
        url: reqwest::Url,
        port: Option<u16>,
    ) -> Result<PortForwarder, String> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .map_err(|e| format!("opening a loopback port: {e}"))?;
        let address = listener
            .local_addr()
            .map_err(|e| format!("opening a loopback port: {e}"))?;
        let failures = Arc::new(Failures {
            port: match port {
                Some(port) => format!("port {port}, forwarded at {address}"),
                None => format!("the port forwarded at {address}"),
            },
            last: Mutex::default(),
        });
        #[cfg(test)]
        let tasks = Arc::new(AtomicUsize::new(0));
        let mut forwarding = JoinSet::new();
        #[cfg(test)]
        let count = Arc::clone(&tasks);
        let failed = Arc::clone(&failures);
        forwarding.spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let Ok((accepted, _)) = accepted else { break };
                        let (http, url) = (http.clone(), url.clone());
                        let (credentials, headers) = (Arc::clone(&credentials), current(&headers));
                        let failed = Arc::clone(&failed);
                        connections.spawn(async move {
                            // Failed, the connection closes: as a port nothing listens on does.
                            let failure = match credentials.token().await {
                                Err(e) => Some(ForwardError::new(format!(
                                    "no token to reach the server: {e}"
                                ))),
                                Ok(token) => match open(&http, token.as_deref(), headers, url).await {
                                    Ok(socket) => carry(accepted, socket).await.err(),
                                    Err(refused) => {
                                        if refused.status == Some(StatusCode::UNAUTHORIZED.as_u16()) {
                                            credentials.refused();
                                        }
                                        Some(refused)
                                    }
                                },
                            };
                            if let Some(failure) = failure {
                                failed.fail(failure);
                            }
                        });
                    }
                    Some(_) = connections.join_next(), if !connections.is_empty() => {}
                }
                #[cfg(test)]
                count.store(connections.len(), SeqCst);
            }
        });
        Ok(PortForwarder {
            address,
            _forwarding: Some(forwarding),
            failures: Some(failures),
            #[cfg(test)]
            tasks,
        })
    }
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
            status: refusal.status(),
            code: refusal.code().map(str::to_owned),
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
/// messages, and the binary messages to `tcp`. Ended by the server before the port sent a
/// byte, why.
async fn carry(
    mut tcp: TcpStream,
    socket: WebSocketStream<reqwest::Upgraded>,
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
                    answered = true;
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
            "the server closed its WebSocket {before}, as it does when nothing listens on the \
             port in the sandbox"
        ))),
        End::Closed(Some(reason)) => Err(ForwardError::new(format!(
            "the server closed its WebSocket {before}: {reason}"
        ))),
        End::Failed(e) => Err(ForwardError::new(format!(
            "its WebSocket failed {before}: {e}"
        ))),
    }
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
    /// port `deaf` closes its WebSocket at once, and whose every other port is refused. Given
    /// `token`, it wants it as a bearer token.
    async fn serve(token: Option<&'static str>) -> String {
        let app = axum::Router::new().route(
            "/virtx/ports/{capability}",
            axum::routing::get(
                move |Path(capability): Path<String>,
                      headers: HeaderMap,
                      upgrade: WebSocketUpgrade| async move {
                    let offered = headers
                        .get(header::SEC_WEBSOCKET_PROTOCOL)
                        .is_some_and(|value| value == TCP_SUBPROTOCOL);
                    if !["open", "deaf"].contains(&capability.as_str()) || !offered {
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
                    if capability == "deaf" {
                        // Closed at once, as a port nothing listens on is.
                        return upgrade.protocols([TCP_SUBPROTOCOL]).on_upgrade(
                            |mut socket| async move {
                                let _ = socket.send(ws::Message::Close(None)).await;
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
        let forward = PortForwarder::open(&client, "ports/open", None)
            .await
            .unwrap();
        assert!(forward.address().ip().is_loopback());

        // Two at once, each its own WebSocket.
        let mut first = TcpStream::connect(forward.address()).await.unwrap();
        let mut second = TcpStream::connect(forward.address()).await.unwrap();
        for (stream, said) in [(&mut first, b"one"), (&mut second, b"two")] {
            stream.write_all(said).await.unwrap();
            let mut echoed = [0u8; 3];
            stream.read_exact(&mut echoed).await.unwrap();
            assert_eq!(&echoed, said);
        }
        assert_eq!(forward.last_error(), None, "each reached the port");

        // Gone with the forward: looked for a minute, ending once it refuses.
        let address = forward.address();
        drop(forward);
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
        let forward = PortForwarder::open(&client, "ports/shut", Some(7000))
            .await
            .unwrap();
        assert_eq!(forward.last_error(), None);
        let mut stream = TcpStream::connect(forward.address()).await.unwrap();
        let mut read = Vec::new();
        assert_eq!(stream.read_to_end(&mut read).await.unwrap(), 0);

        let failed = failed(&forward).await;
        assert_eq!(failed.status, Some(404));
        assert_eq!(failed.code.as_deref(), Some("not_found"));
        let named = format!("port 7000, forwarded at {}: ", forward.address());
        assert!(failed.message.starts_with(&named), "{}", failed.message);
        assert!(
            failed.message.contains("no such port"),
            "{}",
            failed.message
        );
        // The URL is as secret as the credentials it carries.
        assert!(!failed.message.contains("ports/shut"), "{}", failed.message);
    }

    #[tokio::test]
    async fn a_websocket_closed_before_the_port_sent_a_byte_says_nothing_listens() {
        let client =
            TransportClient::connect(&serve(None).await, Auth::None, HeaderMap::new()).unwrap();
        let forward = PortForwarder::open(&client, "ports/deaf", Some(7000))
            .await
            .unwrap();
        let mut stream = TcpStream::connect(forward.address()).await.unwrap();
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
            failed.message.contains("nothing listens on the port"),
            "{}",
            failed.message
        );
    }

    #[tokio::test]
    async fn a_port_named_by_no_number_is_named_by_where_it_is_forwarded() {
        let client =
            TransportClient::connect(&serve(None).await, Auth::None, HeaderMap::new()).unwrap();
        let forward = PortForwarder::open(&client, "ports/deaf", None)
            .await
            .unwrap();
        let mut stream = TcpStream::connect(forward.address()).await.unwrap();
        let _ = stream.read_to_end(&mut Vec::new()).await;
        let named = format!("the port forwarded at {}: ", forward.address());
        let failed = failed(&forward).await;
        assert!(failed.message.starts_with(&named), "{}", failed.message);
    }

    #[tokio::test]
    async fn a_connection_goes_with_the_auth_current_when_it_is_made() {
        let client =
            TransportClient::connect(&serve(Some("later")).await, Auth::None, HeaderMap::new())
                .unwrap();
        let forward = PortForwarder::open(&client, "ports/open", None)
            .await
            .unwrap();
        let mut stream = TcpStream::connect(forward.address()).await.unwrap();
        let mut read = Vec::new();
        assert_eq!(stream.read_to_end(&mut read).await.unwrap(), 0, "refused");
        assert_eq!(failed(&forward).await.status, Some(401));

        client
            .set_auth(Auth::Bearer {
                token: "later".into(),
            })
            .unwrap();
        let mut stream = TcpStream::connect(forward.address()).await.unwrap();
        stream.write_all(b"now").await.unwrap();
        let mut echoed = [0u8; 3];
        stream.read_exact(&mut echoed).await.unwrap();
        assert_eq!(&echoed, b"now");
    }

    #[tokio::test]
    async fn a_loopback_address_is_reached_as_it_is() {
        let forward = PortForwarder::direct("http://127.0.0.1:54123")
            .await
            .unwrap();
        assert_eq!(forward.address(), "127.0.0.1:54123".parse().unwrap());
        assert!(PortForwarder::direct("ports/abc").await.is_err());
    }

    #[tokio::test]
    async fn completed_connection_tasks_are_reaped() {
        let client =
            TransportClient::connect(&serve(None).await, Auth::None, HeaderMap::new()).unwrap();
        let forward = PortForwarder::open(&client, "ports/open", None)
            .await
            .unwrap();

        for i in 0..10 {
            let mut stream = TcpStream::connect(forward.address()).await.unwrap();
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
        let forward = PortForwarder::open(&client, "ports/shut", None)
            .await
            .unwrap();

        for _ in 0..5 {
            let mut stream = TcpStream::connect(forward.address()).await.unwrap();
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
}

#[cfg(feature = "ffi")]
mod ffi {
    //! [`PortForwarder`] behind the C ABI: opened over a client, on its runtime, and freed.

    use std::ffi::c_int;

    use serde::{Deserialize, Serialize};

    use super::PortForwarder;
    use crate::{
        client::ffi::VirtxClient,
        ffi::{VIRTX_ERROR, VIRTX_OK, VirtxBytes, encode, json, write},
    };

    /// What `virtx_port_forwarder_open` takes: a port's `url`, as the server answered it, and
    /// its number in the sandbox, which names it in what a connection that fails says.
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct PortForwarderArgs {
        url: String,
        #[serde(default)]
        port: Option<u16>,
    }

    /// Where a forwarder is reached, as `host:port`.
    #[derive(Serialize)]
    struct PortForwarderOutput {
        address: String,
    }

    /// A `PortForwarder`, as C holds it: held for the forwarding it does until it is freed.
    pub struct VirtxPortForwarder(PortForwarder);

    /// Forward the port a sandbox answered `url` for, given `{"url": "...", "port"?: n}` as
    /// `virtx_client_create_sandbox` answered it, to this machine's loopback: write the
    /// forwarder to `forwarder`, and where it is to `out`, as `{"address": "127.0.0.1:n"}`, or
    /// the error's message. Blocks until it is open, which takes no request of the server.
    ///
    /// It runs on `client`'s runtime, so the client's last release stops it, and is freed with
    /// `virtx_port_forwarder_free`.
    ///
    /// # Safety
    ///
    /// `client` is as in `virtx_client_list_volume`, and `args` as in
    /// `virtx_client_build_image`. `forwarder` and `out` are writable. Not called from a `wake`,
    /// which runs on the runtime this blocks on.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_port_forwarder_open(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
        forwarder: *mut *mut VirtxPortForwarder,
        out: *mut VirtxBytes,
    ) -> c_int {
        let args = unsafe { json::<PortForwarderArgs>(args, args_len) };
        let opened = unsafe { &(*client).0 }.block_on(|client| async move {
            let args = args?;
            PortForwarder::open(&client.inner, &args.url, args.port)
                .await
                .map_err(|e| e.to_string())
        });
        let output = opened.map(|opened| {
            let address = opened.address().to_string();
            unsafe { *forwarder = Box::into_raw(Box::new(VirtxPortForwarder(opened))) };
            PortForwarderOutput { address }
        });
        let (status, bytes) = encode(output);
        unsafe { write(out, bytes) };
        status
    }

    /// Why the last connection to `forwarder` that reached nothing in the sandbox failed: its
    /// WebSocket refused, or closed before the port sent a byte, as it is when nothing listens
    /// there. `VIRTX_OK` with `null` written to `out` while none has, or `VIRTX_ERROR` with
    /// the failure written as a call's error is, its message naming the port: `{"message",
    /// "status"?, "code"?}`, `status` and `code` the server's refusal of the WebSocket. Each
    /// such failure is also handed to the function `virtx_set_warn` set.
    ///
    /// # Safety
    ///
    /// `forwarder` is one `virtx_port_forwarder_open` wrote, not yet freed. `out` is writable.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_port_forwarder_last_error(
        forwarder: *const VirtxPortForwarder,
        out: *mut VirtxBytes,
    ) -> c_int {
        let (status, bytes) = match unsafe { &(*forwarder).0 }.last_error() {
            None => (VIRTX_OK, b"null".to_vec()),
            Some(failed) => (
                VIRTX_ERROR,
                serde_json::to_vec(&failed).expect("a failure encodes"),
            ),
        };
        unsafe { write(out, bytes) };
        status
    }

    /// Stop `forwarder` and free it. Null does nothing.
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

            let args = br#"{"url": "ports/abc", "port": 7009}"#;
            let mut forwarder = ptr::null_mut();
            let status = unsafe {
                virtx_port_forwarder_open(
                    client,
                    args.as_ptr(),
                    args.len(),
                    &mut forwarder,
                    &mut out,
                )
            };
            assert_eq!(status, crate::ffi::VIRTX_OK);
            let written = unsafe { std::slice::from_raw_parts(out.data, out.len) };
            let address: serde_json::Value = serde_json::from_slice(written).unwrap();
            let address: std::net::SocketAddr =
                address["address"].as_str().unwrap().parse().unwrap();
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
            assert!(!message.contains("ports/abc"), "{message}");
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
