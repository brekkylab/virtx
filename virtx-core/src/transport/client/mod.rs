//! The client's end: the server's endpoints over either transport, which only opening and
//! closing tell apart. It knows no endpoint's protocol; each is built on [`TransportClient::send`].

mod credentials;
mod portforward;
mod stdio;
mod tcp;

use std::io;
use std::time::Duration;

use bytes::Bytes;
use http::{HeaderMap, StatusCode};
use http_body_util::{BodyExt, Full, combinators::BoxBody};

pub use self::portforward::{ForwardError, PortForwarder};
use self::{stdio::Stdio, tcp::Tcp};
use super::http::refused;
use crate::{Auth, Error};

/// A request's or a response's body, as either transport carries it.
pub(crate) type Body = BoxBody<Bytes, io::Error>;

/// `bytes`, as a whole [`Body`].
pub(crate) fn full(bytes: impl Into<Bytes>) -> Body {
    Full::new(bytes.into()).map_err(|e| match e {}).boxed()
}

/// The most a request's body may be to be kept, to send it again ([`TransportClient::send`]).
const REPLAYABLE: u64 = 1 << 20;

/// The longest a request is held back from the server, all told -- its answers' time and the
/// waits between them -- while it turns it away.
const PATIENCE: Duration = Duration::from_secs(30);

/// The wait `response` asks for, in seconds (`Retry-After`; never a date, from these servers).
fn retry_after(response: &http::Response<Body>) -> Option<Duration> {
    let seconds = response.headers().get(http::header::RETRY_AFTER)?;
    Some(Duration::from_secs(
        seconds.to_str().ok()?.trim().parse().ok()?,
    ))
}

/// `response`, a `429`, and whether it is a quota's past which the caller is, as virtx cloud
/// answers one: `{"error": {"code": "quota", "reason"}}`, its reason other than the rate of
/// requests. Its body read, and given back whole.
async fn past_a_quota(response: http::Response<Body>) -> (http::Response<Body>, bool) {
    if response.status() != StatusCode::TOO_MANY_REQUESTS {
        return (response, false);
    }
    let (parts, body) = response.into_parts();
    let bytes = body
        .collect()
        .await
        .map(|b| b.to_bytes())
        .unwrap_or_default();
    let said: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_default();
    let quota = said["error"]["code"] == "quota" && said["error"]["reason"] != "requests";
    (http::Response::from_parts(parts, full(bytes)), quota)
}

/// `response`, a `503` to a request that makes something, and whether it says nothing was made:
/// `{"error": {"code": "capacity"}}`, the room for it not found -- as virtx's servers and virtx
/// cloud's gateway answer one turned away before anything is made. Any other may have come
/// after the making began. Its body read, and given back whole.
async fn nothing_made(response: http::Response<Body>) -> (http::Response<Body>, bool) {
    let (parts, body) = response.into_parts();
    let bytes = body
        .collect()
        .await
        .map(|b| b.to_bytes())
        .unwrap_or_default();
    let said: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_default();
    let nothing = said["error"]["code"] == "capacity";
    (http::Response::from_parts(parts, full(bytes)), nothing)
}

/// How a client reaches the server's endpoints.
enum Connection {
    Tcp(Tcp),
    Stdio(Stdio),
}

impl Connection {
    /// Send `request`, whose URI is a path under the server's base, such as `/volumes/`. An
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
    /// it over its stdin and stdout: whatever `command` set for those two is replaced, and it
    /// runs in a process group of its own, out of reach of the terminal's Ctrl-C. The rest of it
    /// (arguments, environment, directory, stderr) is the caller's. Must be called inside a
    /// tokio runtime.
    pub fn spawn(command: impl Into<tokio::process::Command>) -> io::Result<Self> {
        Stdio::spawn(command).map(|stdio| Self::new(Connection::Stdio(stdio)))
    }

    /// Reach the server at `url`, its base `http` or `https` URL, sending `auth`, the
    /// credentials it wants, and `headers`, whatever else it asks of each request, with every
    /// request. `Authorization` is `auth`'s, over one given as a header. Nothing is sent until
    /// the first request. Refused when a token would go in the clear: over `http` to any host
    /// but this machine.
    pub fn connect(url: &str, auth: Auth, headers: HeaderMap) -> anyhow::Result<Self> {
        Tcp::connect(url, auth, headers).map(|tcp| Self::new(Connection::Tcp(tcp)))
    }

    /// Send `auth` with every request from now on, in place of what was given at
    /// [`connect`](Self::connect): a new token, when the server's are short-lived. A forwarder
    /// already open sends it with its next connection. Over stdio, where none is sent, nothing
    /// changes. Refused as at `connect`, the auth unchanged.
    pub fn set_auth(&self, auth: Auth) -> anyhow::Result<()> {
        match &self.connection {
            Connection::Tcp(tcp) => tcp.set_auth(auth),
            Connection::Stdio(_) => Ok(()),
        }
    }

    /// Send `headers` with every request from now on, in place of those given at
    /// [`connect`](Self::connect). A forwarder already open sends them with its next
    /// connection. Over stdio, where none are sent, nothing changes.
    pub fn set_headers(&self, headers: HeaderMap) {
        if let Connection::Tcp(tcp) = &self.connection {
            tcp.set_headers(headers);
        }
    }

    /// A client of the server at the other end of `io`, one HTTP/2 connection as over stdio.
    #[cfg(any(feature = "test-util", all(test, feature = "server")))]
    pub(crate) fn over<T>(io: T) -> Self
    where
        T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin + 'static,
    {
        Self::new(Connection::Stdio(Stdio::over(io, None)))
    }

    /// Send `request` to whichever endpoint its URI, a path under the server's base such as
    /// `/volumes/`, names. An `Err` says why no response came, or that the server refused the
    /// headers with a `401` or a `403`, which every endpoint's are alike.
    ///
    /// A request whose body is known and small ([`REPLAYABLE`]) -- every resource's, and a
    /// registry's reads -- is sent again when the server turns it away for now: a `429` or a
    /// `503`, after its `Retry-After` or else a wait that doubles, for [`PATIENCE`] at most, its
    /// answers' time counted with the waits; and a `401`, at once and once, when the token is a
    /// command's, which prints a new one. The server did nothing with a request it turned away,
    /// so sending it again is safe -- but a `503` to one that makes something (a `POST` or a
    /// `PATCH`) may have come after the making began: sent again only where it says nothing was
    /// made ([`nothing_made`]), so a retry never makes a second of what the first made.
    ///
    /// A path, or a `Destination`, with a `.` or `..` segment still in it, a file's path that
    /// goes above its root or a name that is one, is refused unsent: over HTTP it would be
    /// resolved before it went, to another endpoint's path even, and the server refuses one that
    /// arrives.
    pub(crate) async fn send(
        &self,
        request: http::Request<Body>,
    ) -> Result<http::Response<Body>, Error> {
        if let Some(path) = dot_segment(&request) {
            return Err(Error::Path(format!(
                "{path:?}: a path cannot go above its root, nor a name be \".\" or \"..\""
            )));
        }
        let (parts, body) = request.into_parts();
        let small = hyper::body::Body::size_hint(&body)
            .exact()
            .is_some_and(|size| size <= REPLAYABLE);
        if !small {
            return self.send_once(http::Request::from_parts(parts, body)).await;
        }
        let bytes = body
            .collect()
            .await
            .map_err(|e| Error::Transport(e.to_string()))?
            .to_bytes();
        let started = tokio::time::Instant::now();
        let makes = matches!(parts.method, http::Method::POST | http::Method::PATCH);
        let mut backoff = Duration::from_millis(500);
        let mut renewed = false;
        loop {
            let mut request = http::Request::new(full(bytes.clone()));
            *request.method_mut() = parts.method.clone();
            *request.uri_mut() = parts.uri.clone();
            *request.version_mut() = parts.version;
            *request.headers_mut() = parts.headers.clone();
            let response = match self.send_once(request).await {
                Err(Error::Status(401, _)) if !renewed && self.renews() => {
                    renewed = true;
                    continue;
                }
                answered => answered?,
            };
            let status = response.status();
            if status != StatusCode::TOO_MANY_REQUESTS && status != StatusCode::SERVICE_UNAVAILABLE
            {
                return Ok(response);
            }
            // A quota past which the caller is -- sandboxes open, storage -- frees up only as it
            // closes or deletes something, which waiting here does not do: answered at once. The
            // rate of requests, which frees up by itself, is waited out.
            let (response, quota) = past_a_quota(response).await;
            if quota {
                return Ok(response);
            }
            let response = match status == StatusCode::SERVICE_UNAVAILABLE && makes {
                true => match nothing_made(response).await {
                    (response, true) => response,
                    (response, false) => return Ok(response),
                },
                false => response,
            };
            let wait = retry_after(&response).unwrap_or(backoff).min(PATIENCE);
            if started.elapsed() + wait > PATIENCE {
                return Ok(response);
            }
            tokio::time::sleep(wait).await;
            backoff *= 2;
        }
    }

    async fn send_once(&self, request: http::Request<Body>) -> Result<http::Response<Body>, Error> {
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

    /// Whether a token refused is printed anew: an [`Auth::Command`]'s.
    fn renews(&self) -> bool {
        match &self.connection {
            Connection::Tcp(tcp) => tcp.credentials.renews(),
            Connection::Stdio(_) => false,
        }
    }

    /// End the session. Over stdio, close the server's stdin and kill it if it has not exited
    /// within five seconds; over HTTP there is nothing to end.
    pub async fn close(self) {
        if let Connection::Stdio(stdio) = self.connection {
            stdio.close().await;
        }
    }
}

/// The path of `request`, or its `Destination`, if either has a `.` or `..` segment, plain or
/// percent-encoded, with `\` separating segments too, as an `http` URL reads one.
fn dot_segment(request: &http::Request<Body>) -> Option<&str> {
    let is_dot = |segment: &str| {
        matches!(
            segment.to_ascii_lowercase().replace("%2e", ".").as_str(),
            "." | ".."
        )
    };
    let destination = request
        .headers()
        .get("Destination")
        .and_then(|to| to.to_str().ok());
    [Some(request.uri().path()), destination]
        .into_iter()
        .flatten()
        .find(|path| path.split(['/', '\\']).any(is_dot))
}
