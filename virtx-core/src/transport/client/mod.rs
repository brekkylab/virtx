//! The client's end: the server's endpoints over either transport, which only opening and
//! closing tell apart. It knows no endpoint's protocol; each is built on [`TransportClient::send`].

mod portforward;
mod stdio;
mod tcp;

use std::io;

use bytes::Bytes;
use http::{HeaderMap, StatusCode};
use http_body_util::{BodyExt, Full, combinators::BoxBody};

pub use self::portforward::PortForwarder;
use self::{stdio::Stdio, tcp::Tcp};
use crate::Error;

/// A request's or a response's body, as either transport carries it.
pub(crate) type Body = BoxBody<Bytes, io::Error>;

/// `bytes`, as a whole [`Body`].
pub(crate) fn full(bytes: impl Into<Bytes>) -> Body {
    Full::new(bytes.into()).map_err(|e| match e {}).boxed()
}

/// `response`, a refusal, as the [`Error::Status`] it is: its body read, for a person.
pub(crate) async fn refused(response: http::Response<Body>) -> Error {
    let status = response.status().as_u16();
    let body = response
        .into_body()
        .collect()
        .await
        .map(|body| body.to_bytes())
        .unwrap_or_default();
    Error::Status(status, String::from_utf8_lossy(&body).trim().to_owned())
}

/// How a client reaches the server's endpoints.
enum Connection {
    Tcp(Tcp),
    Stdio(Stdio),
}

impl Connection {
    /// Send `request`, whose URI is a path under the server's base, such as `/rpc`. An
    /// `Err` says why no response came.
    async fn send(&self, request: http::Request<Body>) -> Result<http::Response<Body>, String> {
        match self {
            Connection::Tcp(tcp) => tcp.send(request).await,
            Connection::Stdio(stdio) => stdio.send(request).await,
        }
    }
}

/// A client of one server, over stdio or HTTP: the connection every endpoint's requests go by,
/// and all they share.
pub struct TransportClient {
    connection: Connection,
}

impl TransportClient {
    fn new(connection: Connection) -> Self {
        TransportClient { connection }
    }

    /// Start `command`, a [`std::process::Command`] or a tokio one, as the server, and talk to
    /// it over its stdin and stdout: whatever `command` set for those two is replaced. The rest
    /// of it (arguments, environment, directory, stderr) is the caller's. Must be called inside
    /// a tokio runtime.
    pub fn spawn(command: impl Into<tokio::process::Command>) -> io::Result<Self> {
        Stdio::spawn(command).map(|stdio| Self::new(Connection::Stdio(stdio)))
    }

    /// Reach the server at `url`, its base `http` or `https` URL, sending `headers` with every
    /// request: what the server asks of each, such as the `Authorization` its credentials go
    /// in. Nothing is sent until the first request.
    pub fn connect(url: &str, headers: HeaderMap) -> anyhow::Result<Self> {
        Tcp::connect(url, headers).map(|tcp| Self::new(Connection::Tcp(tcp)))
    }

    /// Send `headers` with every request from now on, in place of those given at
    /// [`connect`](Self::connect): new credentials, when the server's are short-lived. A
    /// forwarder already open sends them with its next connection. Over stdio, where none
    /// are sent, nothing changes.
    pub fn set_headers(&self, headers: HeaderMap) {
        if let Connection::Tcp(tcp) = &self.connection {
            tcp.set_headers(headers);
        }
    }

    /// A client of the server at the other end of `io`, one HTTP/2 connection as over stdio.
    #[cfg(all(test, feature = "server"))]
    pub(crate) fn over<T>(io: T) -> Self
    where
        T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin + 'static,
    {
        Self::new(Connection::Stdio(Stdio::over(io, None)))
    }

    /// Send `request` to whichever endpoint its URI, a path under the server's base such as
    /// `/rpc`, names. An `Err` says why no response came, or that the server refused the
    /// headers with a `401` or a `403`, which every endpoint's are alike.
    pub(crate) async fn send(
        &self,
        request: http::Request<Body>,
    ) -> Result<http::Response<Body>, Error> {
        let response = self
            .connection
            .send(request)
            .await
            .map_err(Error::Transport)?;
        let status = response.status();
        if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
            return Err(refused(response).await);
        }
        Ok(response)
    }

    /// End the session. Over stdio, close the server's stdin and kill it if it has not exited
    /// within five seconds; over HTTP there is nothing to end.
    pub async fn close(self) {
        if let Connection::Stdio(stdio) = self.connection {
            stdio.close().await;
        }
    }
}
