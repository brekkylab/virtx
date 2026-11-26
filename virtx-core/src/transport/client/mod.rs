//! The client's end: the server's endpoints over either transport, which only opening and
//! closing tell apart. It knows no endpoint's protocol; each is built on [`TransportClient::send`].

mod credentials;
mod portforward;
mod stdio;
mod tcp;

use std::io;
use std::sync::{PoisonError, RwLock};
use std::time::Duration;

use bytes::Bytes;
use http::{HeaderMap, HeaderValue, StatusCode};
use http_body_util::{BodyExt, Full, combinators::BoxBody};

pub use self::portforward::{ForwardError, PortForwarder};
use self::{stdio::Stdio, tcp::Tcp};
use super::http::refused;
use crate::{Auth, Error, PROJECT_HEADER};

/// A request's or a response's body, as either transport carries it.
pub(crate) type Body = BoxBody<Bytes, io::Error>;

/// `bytes`, as a whole [`Body`].
pub(crate) fn full(bytes: impl Into<Bytes>) -> Body {
    Full::new(bytes.into()).map_err(|e| match e {}).boxed()
}

/// The most a request's body may be to be kept, to send it again ([`TransportClient::send`]).
const REPLAYABLE: u64 = 1 << 20;

/// The longest a request is held back from the server, all told -- its answers' time and the
/// waits between them -- while it turns it away for now.
const PATIENCE: Duration = Duration::from_secs(30);

/// The first wait before a request turned away without a `Retry-After` is sent again, each
/// after twice the one before; and the least wait, whatever `Retry-After` asks.
const FIRST_WAIT: Duration = Duration::from_millis(500);

/// The wait `response` asks for, in seconds (`Retry-After`; never a date, from these servers).
fn retry_after(response: &http::Response<Body>) -> Option<Duration> {
    let seconds = response.headers().get(http::header::RETRY_AFTER)?;
    Some(Duration::from_secs(
        seconds.to_str().ok()?.trim().parse().ok()?,
    ))
}

/// What a `429` or a `503` says of the request it answers, as the protocol's Refused for now has
/// it.
struct ForNow {
    /// Whether to send the request again after a wait: a `429` unless it is a `quota` whose
    /// `reason` is other than `requests`, which frees up only as the caller closes or deletes
    /// something; a `503` where it is `capacity`, or where the request is `repeatable`.
    again: bool,
    /// Whether nothing of the request was done: a `429`, or a `503` that is `capacity`. Any
    /// other `503` may have come after it was carried out.
    undone: bool,
}

/// `response`, a `429` or a `503`, and what it says of a request that is `repeatable` or not
/// ([`TransportClient::send`]). Its body read, and given back whole.
async fn for_now(
    response: http::Response<Body>,
    repeatable: bool,
) -> (http::Response<Body>, ForNow) {
    let status = response.status();
    let (parts, body) = response.into_parts();
    let bytes = body
        .collect()
        .await
        .map(|b| b.to_bytes())
        .unwrap_or_default();
    let said: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_default();
    let error = &said["error"];
    let said = match status {
        StatusCode::TOO_MANY_REQUESTS => ForNow {
            again: error["code"] != "quota" || error["reason"] == "requests",
            undone: true,
        },
        _ => {
            let undone = error["code"] == "capacity";
            ForNow {
                again: undone || repeatable,
                undone,
            }
        }
    };
    (http::Response::from_parts(parts, full(bytes)), said)
}

/// How a client reaches the server's endpoints.
enum Connection {
    Tcp(Tcp),
    Stdio(Stdio),
}

impl Connection {
    /// Send `request`, whose URI is a path under the server's base. An `Err` says why no
    /// response came.
    async fn send(&self, request: http::Request<Body>) -> Result<http::Response<Body>, Error> {
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
    /// The project every request names, over either transport, in place of one among the
    /// headers given: the user's own project while there is none.
    project: RwLock<Option<HeaderValue>>,
}

impl TransportClient {
    fn new(connection: Connection) -> Self {
        TransportClient {
            connection,
            project: RwLock::new(None),
        }
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
    pub fn connect(url: &str, auth: Auth, headers: HeaderMap) -> Result<Self, Error> {
        Tcp::connect(url, auth, headers).map(|tcp| Self::new(Connection::Tcp(tcp)))
    }

    /// Send `auth` with every request from now on, in place of what was given at
    /// [`connect`](Self::connect): a new token, when the server's are short-lived. A forwarder
    /// already open sends it with its next connection. Over stdio, where none is sent, nothing
    /// changes. Refused as at `connect`, the auth unchanged.
    pub fn set_auth(&self, auth: Auth) -> Result<(), Error> {
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

    /// Send the requests from now on through `connections` HTTP clients in turn, one until this
    /// says otherwise: over HTTPS, each one HTTP/2 connection where the server speaks it, so that
    /// a burst of requests is that many TLS handshakes, its streams spread over them. Over stdio,
    /// one connection already, nothing changes. Refused for none.
    pub fn set_connections(&self, connections: usize) -> Result<(), Error> {
        match &self.connection {
            Connection::Tcp(tcp) => tcp.set_connections(connections),
            Connection::Stdio(_) if connections == 0 => {
                Err(Error::Client("connections: 0 sends nothing".into()))
            }
            Connection::Stdio(_) => Ok(()),
        }
    }

    /// Name `project` in every request from now on, in place of a `virtx-project` among the
    /// headers given. With `None`, that header names it where there is one, and otherwise none
    /// is named, for the user's own project. Refused for a name no header carries, the project
    /// unchanged.
    pub fn set_project(&self, project: Option<&str>) -> Result<(), Error> {
        let project = project
            .map(HeaderValue::from_str)
            .transpose()
            .map_err(|_| Error::Client(format!("{project:?} is no project's name")))?;
        *self.project.write().unwrap_or_else(PoisonError::into_inner) = project;
        Ok(())
    }

    /// A client of the server at the other end of `io`, one HTTP/2 connection as over stdio.
    #[cfg(any(feature = "test-util", all(test, feature = "server")))]
    pub(crate) fn over<T>(io: T) -> Self
    where
        T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin + 'static,
    {
        Self::new(Connection::Stdio(Stdio::over(io, None)))
    }

    /// Send `request` to whichever endpoint its URI, a path under the server's base, names. An
    /// `Err` says why no response came, or that the server refused the headers with a `401` or
    /// a `403`, which every endpoint's are alike. A path, or a
    /// `Destination`, with a `.` or `..` segment still in it, a file's path that goes above its
    /// root or a name that is one, is refused unsent: over HTTP it would be resolved before it
    /// went, to another endpoint's path even, and the server refuses one that arrives.
    ///
    /// A request whose body is known and small ([`REPLAYABLE`]) -- every resource's, and a
    /// small file's -- is sent again when the server turns it away for now: a `429` or a `503`,
    /// after its `Retry-After` or else a wait that doubles, for [`PATIENCE`] at most, its
    /// answers' time counted with the waits; and a `401`, at once and once, when the token is a
    /// command's, which prints a new one. A refusal that waiting does not change is answered at
    /// once, and one that may have come after the request was carried out is sent again only
    /// where a second sending leaves what the first did: a `GET`, `HEAD`, `OPTIONS`, `PUT`,
    /// `DELETE` or `PROPFIND` with no `If-Match` or `If-None-Match` ([`for_now`]). Such a
    /// `DELETE` that then finds nothing, `404`, is answered `204`: what it names is gone.
    pub(crate) async fn send(
        &self,
        request: http::Request<Body>,
    ) -> Result<http::Response<Body>, Error> {
        if let Some(path) = dot_segment(&request) {
            return Err(Error::Client(format!(
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
        let conditional = [http::header::IF_MATCH, http::header::IF_NONE_MATCH]
            .iter()
            .any(|condition| parts.headers.contains_key(condition));
        let repeatable = !conditional
            && matches!(
                parts.method.as_str(),
                "GET" | "HEAD" | "OPTIONS" | "PUT" | "DELETE" | "PROPFIND"
            );
        let mut backoff = FIRST_WAIT;
        let mut renewed = false;
        // Whether a sending before may have been carried out.
        let mut maybe_done = false;
        loop {
            let mut request = http::Request::new(full(bytes.clone()));
            *request.method_mut() = parts.method.clone();
            *request.uri_mut() = parts.uri.clone();
            *request.version_mut() = parts.version;
            *request.headers_mut() = parts.headers.clone();
            let response = match self.send_once(request).await {
                Err(refused) if refused.status() == Some(401) && !renewed && self.renews() => {
                    renewed = true;
                    continue;
                }
                answered => answered?,
            };
            let status = response.status();
            if status == StatusCode::NOT_FOUND && maybe_done && parts.method == http::Method::DELETE
            {
                return Ok(http::Response::builder()
                    .status(StatusCode::NO_CONTENT)
                    .body(full(Bytes::new()))
                    .expect("a valid response"));
            }
            if status != StatusCode::TOO_MANY_REQUESTS && status != StatusCode::SERVICE_UNAVAILABLE
            {
                return Ok(response);
            }
            let (response, said) = for_now(response, repeatable).await;
            let wait = retry_after(&response)
                .unwrap_or(backoff)
                .clamp(FIRST_WAIT, PATIENCE);
            if !said.again || started.elapsed() + wait > PATIENCE {
                return Ok(response);
            }
            maybe_done |= !said.undone;
            tokio::time::sleep(wait).await;
            backoff *= 2;
        }
    }

    async fn send_once(
        &self,
        mut request: http::Request<Body>,
    ) -> Result<http::Response<Body>, Error> {
        let project = self
            .project
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some(project) = project {
            request.headers_mut().insert(PROJECT_HEADER, project);
        }
        let response = self.connection.send(request).await?;
        let status = response.status();
        if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
            return Err(refused(response).await);
        }
        Ok(response)
    }

    /// Whether a connection that breaks is made anew by the next request: over TCP. Over stdio,
    /// the one connection is the server's process, and gone with it.
    pub(crate) fn reconnects(&self) -> bool {
        matches!(self.connection, Connection::Tcp(_))
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

#[cfg(all(test, unix))]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[tokio::test]
    async fn a_request_refused_a_command_s_token_goes_again_with_the_one_printed_anew() {
        let requests = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&requests);
        let router = axum::Router::new().fallback(move |headers: HeaderMap| {
            counted.fetch_add(1, Ordering::Relaxed);
            let token = headers.get(http::header::AUTHORIZATION).cloned();
            async move {
                match token.is_some_and(|token| token == "Bearer new") {
                    true => StatusCode::NO_CONTENT,
                    false => StatusCode::UNAUTHORIZED,
                }
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, router).await });

        let script = r#"if [ -n "$VIRTX_TOKEN_REFUSED" ]; then echo new; else echo old; fi"#;
        let auth = Auth::Command {
            program: "sh".into(),
            args: vec!["-c".into(), script.into()],
        };
        let client = TransportClient::connect(&url, auth, HeaderMap::new()).unwrap();
        let request = || http::Request::get("/volumes/").body(full("")).unwrap();
        assert_eq!(client.send(request()).await.unwrap().status(), 204);
        assert_eq!(requests.load(Ordering::Relaxed), 2);
        assert_eq!(client.send(request()).await.unwrap().status(), 204);
        assert_eq!(requests.load(Ordering::Relaxed), 3);

        // A token given is the caller's to replace: refused, the request is not sent again.
        let given = Auth::Bearer {
            token: "old".into(),
        };
        let client = TransportClient::connect(&url, given, HeaderMap::new()).unwrap();
        assert_eq!(
            client.send(request()).await.unwrap_err().status(),
            Some(401)
        );
        assert_eq!(requests.load(Ordering::Relaxed), 4);
    }
}
