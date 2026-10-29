//! A sandbox's port, forwarded to this machine's loopback: what `sandbox.init` answered for it,
//! as an address any TCP client connects to.
//!
//! A server on this machine answers a loopback address already, which is reached as it is. A
//! server over HTTP answers a URL relative to its base, which only a WebSocket of the
//! subprotocol `virtx.tcp` opens: so each connection to a loopback port of this client's own
//! is carried in one, its bytes as binary messages, opened by the same HTTP client the requests
//! go by.

use std::net::{Ipv4Addr, SocketAddr};

use super::{Connection, TransportClient};
use crate::Error;
use futures_util::{SinkExt, StreamExt};
use http::{StatusCode, header};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::JoinSet,
};
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{
        Message,
        handshake::{client::generate_key, derive_accept_key},
        protocol::Role,
    },
};

/// The WebSocket subprotocol a port's TCP is carried in.
const TCP_SUBPROTOCOL: &str = "virtx.tcp";

/// How much of a port's TCP one message carries at most.
const CHUNK: usize = 64 << 10;

/// A sandbox's port at an address on this machine's loopback, for as long as this is held. It
/// runs on the runtime it was opened in, and stops with it.
pub struct PortForwarder {
    address: SocketAddr,
    /// What takes the address's connections and carries each, ended when this drops; nothing
    /// where the address is the server's own.
    _forwarding: Option<JoinSet<()>>,
}

impl PortForwarder {
    /// The port a sandbox answered `url` for, as `sandbox.init` and `sandbox.list` answer one,
    /// at an address on this machine's loopback, reached over `client`'s connection.
    ///
    /// Over stdio the server is on this machine, and its loopback address is the forwarder's.
    /// Over HTTP `url` is resolved against the server's base, and each connection to a loopback
    /// port of the forwarder's own is carried to it in a WebSocket of `virtx.tcp`.
    pub async fn open(client: &TransportClient, url: &str) -> Result<PortForwarder, Error> {
        match &client.connection {
            Connection::Stdio(_) => PortForwarder::direct(url).await,
            Connection::Tcp(tcp) => match tcp.base.join(url) {
                Ok(url) => PortForwarder::tunnel(tcp.http.clone(), url).await,
                Err(e) => Err(format!("{url}: {e}")),
            },
        }
        .map_err(Error::Transport)
    }

    /// Where the port is reached: on this machine's loopback.
    pub fn address(&self) -> SocketAddr {
        self.address
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
        })
    }

    /// A loopback port of this client's own, each connection to it carried to `url` in a
    /// WebSocket that `http` opens.
    async fn tunnel(http: reqwest::Client, url: reqwest::Url) -> Result<PortForwarder, String> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .map_err(|e| format!("opening a loopback port: {e}"))?;
        let address = listener
            .local_addr()
            .map_err(|e| format!("opening a loopback port: {e}"))?;
        let mut forwarding = JoinSet::new();
        forwarding.spawn(async move {
            let mut connections = JoinSet::new();
            while let Ok((accepted, _)) = listener.accept().await {
                let (http, url) = (http.clone(), url.clone());
                connections.spawn(async move {
                    // Refused, the connection closes: as a port nothing listens on does.
                    if let Ok(socket) = open(&http, url).await {
                        carry(accepted, socket).await;
                    }
                });
            }
        });
        Ok(PortForwarder {
            address,
            _forwarding: Some(forwarding),
        })
    }
}

/// A WebSocket of `virtx.tcp` to `url`, its handshake an HTTP/1.1 upgrade `http` sends.
async fn open(
    http: &reqwest::Client,
    url: reqwest::Url,
) -> Result<WebSocketStream<reqwest::Upgraded>, String> {
    let key = generate_key();
    let response = http
        .get(url)
        .header(header::CONNECTION, "Upgrade")
        .header(header::UPGRADE, "websocket")
        .header(header::SEC_WEBSOCKET_VERSION, "13")
        .header(header::SEC_WEBSOCKET_KEY, &key)
        .header(header::SEC_WEBSOCKET_PROTOCOL, TCP_SUBPROTOCOL)
        .send()
        .await
        .map_err(|e| format!("{e:#}"))?;
    if response.status() != StatusCode::SWITCHING_PROTOCOLS {
        return Err(format!("the server answered {}", response.status()));
    }
    let accepted = response
        .headers()
        .get(header::SEC_WEBSOCKET_ACCEPT)
        .is_some_and(|accept| accept.as_bytes() == derive_accept_key(key.as_bytes()).as_bytes());
    if !accepted {
        return Err("the server did not accept the WebSocket".into());
    }
    let upgraded = response.upgrade().await.map_err(|e| format!("{e:#}"))?;
    Ok(WebSocketStream::from_raw_socket(upgraded, Role::Client, None).await)
}

/// Carry bytes between `tcp` and `socket` until either ends: what is read from `tcp` as binary
/// messages, and the binary messages to `tcp`.
async fn carry(mut tcp: TcpStream, socket: WebSocketStream<reqwest::Upgraded>) {
    let (mut sink, mut messages) = socket.split();
    let (mut from, mut to) = tcp.split();
    let mut buffer = vec![0u8; CHUNK];
    loop {
        tokio::select! {
            read = from.read(&mut buffer) => match read {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if sink.send(Message::binary(buffer[..n].to_vec())).await.is_err() {
                        break;
                    }
                }
            },
            received = messages.next() => match received {
                Some(Ok(Message::Binary(bytes))) => {
                    if to.write_all(&bytes).await.is_err() {
                        break;
                    }
                }
                // Answered by the socket itself.
                Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => {}
                // Text is no TCP's, and anything else is the end.
                Some(Ok(Message::Text(_) | Message::Close(_)) | Err(_)) | None => break,
            },
        }
    }
    let _ = sink.send(Message::Close(None)).await;
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use axum::{
        extract::{
            Path,
            ws::{self, WebSocketUpgrade},
        },
        response::IntoResponse,
    };
    use http::HeaderMap;

    use super::*;

    /// A server under the base path `/virtx/`, whose port `open` echoes what it is sent and
    /// whose every other port is refused.
    async fn serve() -> String {
        let app =
            axum::Router::new().route(
                "/virtx/ports/{capability}",
                axum::routing::get(
                    |Path(capability): Path<String>,
                     headers: HeaderMap,
                     upgrade: WebSocketUpgrade| async move {
                        let offered = headers
                            .get(header::SEC_WEBSOCKET_PROTOCOL)
                            .is_some_and(|value| value == TCP_SUBPROTOCOL);
                        if capability != "open" || !offered {
                            return StatusCode::NOT_FOUND.into_response();
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
        let client = TransportClient::connect(&serve().await).unwrap();
        let forward = PortForwarder::open(&client, "ports/open").await.unwrap();
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

        // Gone with the forward.
        let address = forward.address();
        drop(forward);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(TcpStream::connect(address).await.is_err());
    }

    #[tokio::test]
    async fn a_connection_the_server_refuses_is_closed() {
        let client = TransportClient::connect(&serve().await).unwrap();
        let forward = PortForwarder::open(&client, "ports/shut").await.unwrap();
        let mut stream = TcpStream::connect(forward.address()).await.unwrap();
        let mut read = Vec::new();
        assert_eq!(stream.read_to_end(&mut read).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn a_loopback_address_is_reached_as_it_is() {
        let forward = PortForwarder::direct("http://127.0.0.1:54123")
            .await
            .unwrap();
        assert_eq!(forward.address(), "127.0.0.1:54123".parse().unwrap());
        assert!(PortForwarder::direct("ports/abc").await.is_err());
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
        ffi::{VirtxBytes, encode, json, write},
    };

    /// What `virtx_port_forwarder_open` takes: a port's `url`, as the server answered it.
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct PortForwarderArgs {
        url: String,
    }

    /// Where a forwarder is reached, as `host:port`.
    #[derive(Serialize)]
    struct PortForwarderOutput {
        address: String,
    }

    /// A `PortForwarder`, as C holds it: held for the forwarding it does until it is freed.
    pub struct VirtxPortForwarder(#[allow(dead_code)] PortForwarder);

    /// Forward the port a sandbox answered `url` for, given `{"url": "..."}` as
    /// `virtx_client_init_sandbox` or `virtx_client_list_sandbox` answered it, to this machine's
    /// loopback: write the forwarder to `forwarder`, and where it is to `out`, as
    /// `{"address": "127.0.0.1:n"}`, or the error's message. Blocks until it is open.
    ///
    /// It runs on `client`'s runtime, so the client's last release stops it, and is freed with
    /// `virtx_port_forwarder_free`.
    ///
    /// # Safety
    ///
    /// `client` is as in `virtx_client_list_snapshot`, and `args` as in
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
            PortForwarder::open(&client.inner, &args?.url)
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

        #[test]
        fn a_forwarder_answers_its_loopback_address_until_it_is_freed() {
            let mut client = ptr::null_mut();
            let mut out = VirtxBytes {
                data: ptr::null_mut(),
                len: 0,
            };
            // Nothing listens there: each connection to the forwarder is closed.
            let url = c"http://127.0.0.1:9/";
            let status = unsafe { virtx_client_connect(url.as_ptr(), &mut client, &mut out) };
            assert_eq!(status, crate::ffi::VIRTX_OK);

            let args = br#"{"url": "ports/abc"}"#;
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

            unsafe { virtx_port_forwarder_free(forwarder) };
            unsafe { virtx_client_release(client) };
            assert!(std::net::TcpStream::connect(address).is_err());
        }
    }
}
