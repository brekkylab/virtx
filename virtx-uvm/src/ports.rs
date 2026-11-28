//! The ports endpoints of the protocol, each a WebSocket of the subprotocol `virtx.tcp`: a
//! connection to a sandbox's port in its machine at `/sandboxes/{id}/ports/{port}` ([`port`]),
//! and its port on local taken at `/sandboxes/{id}/client-ports/{port}` ([`client_port`]). A
//! server routes each to its sandbox with any method: over HTTP/2 a WebSocket opens with an
//! extended `CONNECT` (RFC 8441), over HTTP/1.1 with a `GET`.

use std::sync::Arc;
use std::time::Duration;

use axum::{
    body::Bytes,
    extract::ws::{
        CloseFrame, Message, WebSocket, WebSocketUpgrade, close_code,
        rejection::WebSocketUpgradeRejection,
    },
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

use virtx_core::PortSide;

use crate::{ClientPortTaker, Sandbox, SandboxError};

/// How often a port on local's WebSocket is pinged while no connection comes, so that what stands
/// between it and its client does not take it for idle.
const CLIENT_PORT_KEEPALIVE: Duration = Duration::from_secs(15);

/// How long a port on local's WebSocket has to answer the connection said on it, longer than a
/// client takes to reach its target, before the connection is refused.
const CLIENT_PORT_ANSWER: Duration = Duration::from_secs(20);

/// The WebSocket subprotocol a port's TCP, and a port on local's, is carried in.
const TCP_SUBPROTOCOL: &str = "virtx.tcp";

/// How much of a port's TCP one message carries at most.
const CHUNK: usize = 64 << 10;

/// The WebSocket of `virtx.tcp` a request to a port, in the sandbox or on local, opens; or what it
/// is answered otherwise: `426` unless it opens a WebSocket, and refused
/// [`Invalid`](SandboxError::Invalid) unless of the subprotocol.
fn tcp_websocket(
    upgrade: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
    headers: &HeaderMap,
) -> Result<Result<WebSocketUpgrade, Response>, SandboxError> {
    let Ok(upgrade) = upgrade else {
        return Ok(Err(StatusCode::UPGRADE_REQUIRED.into_response()));
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
        return Err(SandboxError::Invalid(format!(
            "a port is reached over the WebSocket subprotocol {TCP_SUBPROTOCOL}"
        )));
    }
    Ok(Ok(upgrade.protocols([TCP_SUBPROTOCOL])))
}

/// `/sandboxes/{id}/ports/{port}` of `sandbox`, a WebSocket of `virtx.tcp` carrying a TCP
/// connection to the port in its machine: closed at once, saying why, when the port is not
/// reached, such as while no machine runs. Refused as [`tcp_websocket`] says, and
/// [`NotFound`](SandboxError::NotFound) for a port the sandbox was not made with.
pub fn port(
    sandbox: &Arc<Sandbox>,
    port: u16,
    headers: &HeaderMap,
    upgrade: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Result<Response, SandboxError> {
    let opened =
        (sandbox.args().ports.iter()).any(|spec| spec.on == PortSide::Sandbox && spec.port == port);
    if !opened {
        return Err(SandboxError::NotFound(format!(
            "sandbox {} was made with no port {port}",
            sandbox.id()
        )));
    }
    let upgrade = match tcp_websocket(upgrade, headers)? {
        Ok(upgrade) => upgrade,
        Err(refused) => return Ok(refused),
    };
    let activity = sandbox.activity();
    Ok(upgrade.on_upgrade(move |mut socket| async move {
        match activity.sandbox().connect(port).await {
            Ok(reached) => carry(socket, reached).await,
            Err(e) => {
                let frame = CloseFrame {
                    code: close_code::ERROR,
                    reason: close_reason(&e.to_string()).into(),
                };
                let _ = socket.send(Message::Close(Some(frame))).await;
            }
        }
    }))
}

/// Carry bytes between `socket` and `reached` until either ends: what is read from `reached` as
/// binary messages, and the binary messages to it.
async fn carry(mut socket: WebSocket, mut reached: TcpStream) {
    let (mut from, mut to) = reached.split();
    let mut buffer = vec![0u8; CHUNK];
    loop {
        tokio::select! {
            read = from.read(&mut buffer) => match read {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let message = Message::Binary(Bytes::copy_from_slice(&buffer[..n]));
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
    let _ = socket.send(Message::Close(None)).await;
}

/// `said` as a close's reason, which a WebSocket holds 123 bytes of: cut at a character's end.
fn close_reason(said: &str) -> String {
    let mut end = said.len().min(123);
    while !said.is_char_boundary(end) {
        end -= 1;
    }
    said[..end].to_owned()
}

/// `/sandboxes/{id}/client-ports/{port}` of `sandbox`, a WebSocket of `virtx.tcp`: the port
/// taken by this client while it is open, and the next connection a process makes to it carried
/// in it. The connection is said with an empty binary message; the client answers it with
/// another once it has reached what the port is, and the process's `connect` succeeds then, or
/// closes the WebSocket, which refuses it. Refused as [`tcp_websocket`] says, and
/// [`NotFound`](SandboxError::NotFound) for a port the sandbox was not made with: before the port
/// is taken, so a request refused takes no connection.
pub fn client_port(
    sandbox: Arc<Sandbox>,
    port: u16,
    headers: &HeaderMap,
    upgrade: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Result<Response, SandboxError> {
    let upgrade = match tcp_websocket(upgrade, headers)? {
        Ok(upgrade) => upgrade,
        Err(refused) => return Ok(refused),
    };
    let taker = sandbox.take_client_port(port)?;
    Ok(upgrade.on_upgrade(move |socket| take_client_port(socket, sandbox, taker)))
}

/// Wait on `socket` for the next connection `taker` has, and carry it if the client reaches its
/// target in [`CLIENT_PORT_ANSWER`]. A client that closes `socket` meanwhile, or answers no ping
/// before the next, takes none.
async fn take_client_port(mut socket: WebSocket, sandbox: Arc<Sandbox>, taker: ClientPortTaker) {
    let mut pinged = false;
    let connection = loop {
        tokio::select! {
            connection = taker.next() => match connection {
                Some(connection) => break connection,
                None => {
                    let _ = socket.send(Message::Close(None)).await;
                    return;
                }
            },
            received = socket.recv() => match received {
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => pinged = false,
                _ => return,
            },
            () = tokio::time::sleep(CLIENT_PORT_KEEPALIVE) => {
                if pinged || socket.send(Message::Ping(Bytes::new())).await.is_err() {
                    return;
                }
                pinged = true;
            }
        }
    };
    drop(taker);
    if socket.send(Message::Binary(Bytes::new())).await.is_err() {
        return;
    }
    // Dropped unanswered, the connection is refused.
    let answered = tokio::time::timeout(CLIENT_PORT_ANSWER, async {
        loop {
            match socket.recv().await {
                Some(Ok(Message::Binary(said))) if said.is_empty() => return true,
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                _ => return false,
            }
        }
    });
    if answered.await != Ok(true) {
        return;
    }
    let _activity = sandbox.activity();
    let Ok(reached) = connection.accept().await else {
        return;
    };
    carry_halves(socket, reached).await;
}

/// Carry bytes between `socket` and `reached` until both directions have ended: what is read
/// from `reached` as binary messages, and its end as an empty one; the binary messages to it, and
/// an empty one as the client's end, `reached`'s writing shut. Ended at once by a failure, or a
/// close.
async fn carry_halves(mut socket: WebSocket, mut reached: TcpStream) {
    let (mut from, mut to) = reached.split();
    let mut buffer = vec![0u8; CHUNK];
    let (mut read_all, mut written_all) = (false, false);
    while !(read_all && written_all) {
        tokio::select! {
            read = from.read(&mut buffer), if !read_all => match read {
                Ok(n) => {
                    read_all = n == 0;
                    let message = Message::Binary(Bytes::copy_from_slice(&buffer[..n]));
                    if socket.send(message).await.is_err() {
                        break;
                    }
                }
                Err(_) => break,
            },
            received = socket.recv() => match received {
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
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                // Text is no TCP's, and anything else is the end.
                _ => break,
            },
        }
    }
    let _ = socket.send(Message::Close(None)).await;
}
