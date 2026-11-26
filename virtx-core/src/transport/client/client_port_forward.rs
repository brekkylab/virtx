//! A sandbox's client port, taken by this client: what listens on a port of this machine's,
//! carried to the sandbox's processes that connect to it at `10.0.2.2`, as a port forwarded is
//! the other way.
//!
//! A server over HTTP is held a WebSocket of `virtx.tcp` at
//! `/sandboxes/{id}/client-ports/{port}` while the port is taken. A process's connection to the
//! port is said on it with an empty binary message: the target on this machine is connected to
//! then, and the connection answered with another empty message once it is reached, or the
//! WebSocket closed when it is not, which refuses it. Each WebSocket carries one connection, and
//! another is opened to wait for the next as soon as one is said.
//!
//! A server on this machine connects to the port itself, at `127.0.0.1`, as it answers a port of
//! the sandbox's at its own loopback address: taking it is only asking that the sandbox has it.

use std::sync::PoisonError;
use std::time::Duration;

use futures_util::{SinkExt as _, Stream, StreamExt as _};
use http::StatusCode;
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::TcpStream,
    task::JoinSet,
};
use tokio_tungstenite::{WebSocketStream, tungstenite::Message};

use super::{
    Connection, ForwardError, TransportClient, port_forward::open as open_websocket, tcp::current,
};
use crate::{Error, PROJECT_HEADER, transport::segment, warning};

/// How much of a connection's TCP one message carries at most.
const CHUNK: usize = 64 << 10;

/// How long reaching a connection's target may take before it is refused.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// The longest wait before a WebSocket that failed is opened again.
const MOST_WAIT: Duration = Duration::from_secs(8);

/// Take the client port `port` of the sandbox `sandbox` over `client`, each connection to it
/// carried to `target`, `host:port`: `()` once it is taken, and nothing more while it is. Ended
/// with the error that ends it: `404` for a sandbox or a port that is no one's, or once the
/// sandbox is gone. Dropped, the port is let go of, and the connections it carries closed.
///
/// A server on this machine connects to `127.0.0.1:{port}` itself, so a `target` other than that
/// is refused over stdio.
pub(crate) fn take<'a>(
    client: &'a TransportClient,
    sandbox: &str,
    port: u16,
    target: String,
) -> impl Stream<Item = Result<(), Error>> + Send + use<'a> {
    let path = format!("sandboxes/{}/client-ports/{port}", segment(sandbox));
    let sandbox = sandbox.to_owned();
    async_stream::try_stream! {
        let Connection::Tcp(tcp) = &client.connection else {
            if target != format!("127.0.0.1:{port}") {
                Err(Error::Client(format!(
                    "client port {port}: a server on this machine reaches 127.0.0.1:{port} \
                     itself, and no other target"
                )))?;
            }
            let made: crate::Sandbox =
                crate::transport::http::get(client, &format!("/sandboxes/{}", segment(&sandbox)))
                    .await?;
            if !made.client_ports.iter().any(|opened| opened.port == port) {
                Err(Error::Client(format!(
                    "sandbox {sandbox} was made with no client port {port}"
                )))?;
            }
            yield ();
            return futures_util::future::pending().await;
        };
        let url = tcp
            .base
            .join(&path)
            .map_err(|e| Error::Client(format!("{path}: {e}")))?;
        let mut carried = JoinSet::new();
        let (mut opened, mut failed) = (false, 0u32);
        loop {
            while carried.try_join_next().is_some() {}
            let mut sent = current(&tcp.headers);
            let project = client.project.read().unwrap_or_else(PoisonError::into_inner).clone();
            if let Some(project) = project {
                sent.insert(PROJECT_HEADER, project);
            }
            let waiting = match tcp.credentials.token().await {
                Ok(token) => open_websocket(&tcp.http, token.as_deref(), sent, url.clone()).await,
                Err(e) => Err(ForwardError {
                    message: format!("no token to reach the server: {e}"),
                    source: crate::Source::Transport,
                    status: None,
                    code: None,
                }),
            };
            let mut waiting = match waiting {
                Ok(waiting) => waiting,
                Err(refused) => {
                    if refused.status == Some(StatusCode::UNAUTHORIZED.as_u16()) {
                        tcp.credentials.refused();
                    }
                    if !opened || refused.status == Some(StatusCode::NOT_FOUND.as_u16()) {
                        let message = format!("client port {port}: {}", refused.message);
                        Err(match refused.status {
                            Some(status) => Error::Status(status, message, Default::default()),
                            None => Error::Transport(message),
                        })?;
                    }
                    if failed == 0 {
                        warning::warn(format!(
                            "client port {port}: taking it again: {}",
                            refused.message
                        ));
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
                yield ();
            }
            // Until a connection is said on it, or it ends.
            let said = loop {
                match waiting.next().await {
                    Some(Ok(Message::Binary(bytes))) if bytes.is_empty() => break true,
                    Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => {}
                    _ => break false,
                }
            };
            if said {
                carried.spawn(reach(waiting, target.clone(), port));
            }
        }
    }
}

/// Reach `target` for the connection said on `socket`, and carry the one to the other; or, not
/// reached, close `socket`, which refuses it.
async fn reach(mut socket: WebSocketStream<reqwest::Upgraded>, target: String, port: u16) {
    let reached = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(&target)).await;
    let reached = match reached {
        Ok(Ok(reached)) => reached,
        failed => {
            let why = match failed {
                Ok(Err(e)) => e.to_string(),
                _ => "not in time".to_owned(),
            };
            warning::warn(format!(
                "client port {port}: {target} was not reached: {why}"
            ));
            let _ = socket.close(None).await;
            return;
        }
    };
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

#[cfg(all(test, unix))]
mod tests {
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
    use http::HeaderMap;
    use tokio::{net::TcpListener, sync::mpsc};

    use super::*;
    use crate::Auth;

    /// A server over HTTP whose sandbox `s` has the client port 9222: each WebSocket to it is
    /// said one connection at once, and once that is answered it is sent `hi`, and what comes
    /// back is sent to `answered`. Every WebSocket opened is counted in `opened`; another port, or
    /// another sandbox, is `404`.
    async fn serve(opened: Arc<AtomicUsize>, answered: mpsc::UnboundedSender<Vec<u8>>) -> String {
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
    /// WebSocket waits for the next.
    #[tokio::test]
    async fn each_connection_said_is_carried_to_the_target() {
        let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let at = target.local_addr().unwrap().to_string();
        let (answered, mut answers) = mpsc::unbounded_channel();
        let opened = Arc::new(AtomicUsize::new(0));
        let base = serve(Arc::clone(&opened), answered).await;
        let client = TransportClient::connect(&base, Auth::None, HeaderMap::new()).unwrap();
        let mut taken = Box::pin(take(&client, "s", 9222, at));
        let taking = async {
            assert!(matches!(taken.next().await, Some(Ok(()))));
            while taken.next().await.is_some() {}
            panic!("ended");
        };
        let carried = async {
            let (mut reached, _) = target.accept().await.unwrap();
            let mut said = [0u8; 2];
            reached.read_exact(&mut said).await.unwrap();
            assert_eq!(&said, b"hi");
            reached.write_all(b"back").await.unwrap();
            answers.recv().await.unwrap()
        };
        tokio::select! {
            answer = carried => assert_eq!(answer, b"back"),
            () = taking => {}
        }
        assert!(
            opened.load(SeqCst) >= 2,
            "no other WebSocket waits for the next"
        );
    }

    /// A connection whose target is not reached has its WebSocket closed, which refuses it.
    #[tokio::test]
    async fn a_connection_whose_target_is_not_reached_is_refused() {
        let unreached = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let at = unreached.local_addr().unwrap().to_string();
        drop(unreached);
        let (answered, mut answers) = mpsc::unbounded_channel();
        let base = serve(Arc::default(), answered).await;
        let client = TransportClient::connect(&base, Auth::None, HeaderMap::new()).unwrap();
        let mut taken = Box::pin(take(&client, "s", 9222, at));
        tokio::select! {
            _ = async { while taken.next().await.is_some() {} } => panic!("ended"),
            () = tokio::time::sleep(Duration::from_secs(2)) => {}
        }
        assert!(
            answers.try_recv().is_err(),
            "a connection not reached was answered"
        );
    }

    /// A port that is no one's is the stream's error.
    #[tokio::test]
    async fn a_port_the_server_has_not_is_refused() {
        let (answered, _answers) = mpsc::unbounded_channel();
        let base = serve(Arc::default(), answered).await;
        let client = TransportClient::connect(&base, Auth::None, HeaderMap::new()).unwrap();
        let mut taken = Box::pin(take(&client, "s", 5037, "127.0.0.1:5037".into()));
        let refused = taken.next().await.unwrap().unwrap_err();
        assert_eq!(refused.status(), Some(404));
        assert!(taken.next().await.is_none());
    }
}

#[cfg(feature = "ffi")]
mod ffi {
    //! A client port taken behind the C ABI: a stream over a client, on its runtime, freed to
    //! let the port go, as a forwarder is freed.

    use futures_util::StreamExt as _;
    use serde::Deserialize;

    use crate::{
        client::ffi::VirtxClient,
        ffi::{VirtxStream, json},
    };

    /// What `virtx_client_port_forwarder_open` takes.
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct ClientPortArgs {
        sandbox: String,
        port: u16,
        target: Option<String>,
    }

    /// Take the client port `port` of the sandbox `sandbox`, one it was made with in its
    /// `client_ports`, given `{"sandbox": "...", "port": n, "target"?: "host:port"}`: each
    /// connection a process in it makes to `10.0.2.2:{port}` is carried to `target`,
    /// `127.0.0.1:{port}` without one, on this machine, for as long as the stream is held. Its one
    /// item is `null`, once the port is taken; it ends with the error that ends it, such as `404`
    /// for a port the sandbox was not made with or once it is gone. Freeing it lets the port go,
    /// and closes the connections it carries. What fails on the way, such as a target not
    /// reached, is handed to the function `virtx_set_warn` set. A server over stdio, on this
    /// machine, reaches `127.0.0.1:{port}` itself, and refuses another `target`.
    ///
    /// It runs on `client`'s runtime, so the client's last release stops it.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_port_forwarder_open(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxStream {
        let args = unsafe { json::<ClientPortArgs>(args, args_len) };
        unsafe { &(*client).0 }.stream(|client| {
            async_stream::stream! {
                let ClientPortArgs { sandbox, port, target } = match args {
                    Ok(args) => args,
                    Err(error) => {
                        yield Err(error);
                        return;
                    }
                };
                let target = target.unwrap_or_else(|| format!("127.0.0.1:{port}"));
                let mut taken = client.open_client_port(&sandbox, port, target);
                while let Some(item) = taken.next().await {
                    yield item;
                }
            }
        })
    }
}
