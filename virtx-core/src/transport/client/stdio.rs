//! The client's end of stdio: the server as a child process, and one HTTP/2 connection over its
//! stdin and stdout, started with prior knowledge.

use std::{io, process::Stdio as Piped, time::Duration};

use bytes::Bytes;
use futures_util::{
    FutureExt,
    future::{BoxFuture, Shared},
};
use http_body_util::{BodyExt, Full};
use hyper::client::conn::http2::{self, SendRequest};
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    process::{Child, Command},
    sync::oneshot,
    task::JoinHandle,
};

use super::Body;

/// How long a server whose stdin is closed has to exit before it is killed.
const EXIT_GRACE: Duration = Duration::from_secs(5);

/// One HTTP/2 connection to the server, and the server, which dropping kills.
pub(super) struct Stdio {
    /// The connection, once its handshake is done, or why it failed. Not a type alias, which
    /// cbindgen would take for the C ABI's own `Shared`.
    sender: Shared<BoxFuture<'static, Result<SendRequest<Full<Bytes>>, String>>>,
    /// Drives the connection; ending it closes the server's stdin.
    driver: JoinHandle<()>,
    child: Option<Child>,
}

impl Stdio {
    /// Start `command` as the server, and the connection over its stdin and stdout. Must be
    /// called inside a tokio runtime.
    pub(super) fn spawn(command: impl Into<Command>) -> io::Result<Stdio> {
        let mut child = command
            .into()
            .stdin(Piped::piped())
            .stdout(Piped::piped())
            .kill_on_drop(true)
            .spawn()?;
        let stdin = child.stdin.take().expect("piped");
        let stdout = child.stdout.take().expect("piped");
        Ok(Stdio::over(tokio::io::join(stdout, stdin), Some(child)))
    }

    /// The connection over `io`, to `child` if the server is one. Must be called inside a
    /// tokio runtime.
    pub(super) fn over<T>(io: T, child: Option<Child>) -> Stdio
    where
        T: AsyncRead + AsyncWrite + Send + Unpin + 'static,
    {
        let (ready, handshaken) = oneshot::channel();
        let driver = tokio::spawn(async move {
            let handshake = http2::handshake(TokioExecutor::new(), TokioIo::new(io)).await;
            let (sender, connection) = match handshake {
                Ok(handshake) => handshake,
                Err(e) => {
                    let _ = ready.send(Err(format!("connecting to the server: {e}")));
                    return;
                }
            };
            let _ = ready.send(Ok(sender));
            let _ = connection.await;
        });
        let sender = async move {
            handshaken
                .await
                .unwrap_or_else(|_| Err("the server is gone".into()))
        };
        Stdio {
            sender: sender.boxed().shared(),
            driver,
            child,
        }
    }

    /// Send `request`, whose URI is a path such as `rpc`.
    pub(super) async fn send(
        &self,
        request: http::Request<Bytes>,
    ) -> Result<http::Response<Body>, String> {
        let mut sender = self.sender.clone().await?;
        let gone = |e: hyper::Error| format!("the server is gone: {e}");
        sender.ready().await.map_err(gone)?;
        // The authority is not checked.
        let (mut parts, body) = request.into_parts();
        parts.uri = format!("http://virtx/{}", parts.uri)
            .parse()
            .map_err(|e| format!("{e}"))?;
        let request = http::Request::from_parts(parts, Full::new(body));
        let response = sender.send_request(request).await.map_err(gone)?;
        Ok(response.map(|body| body.map_err(io::Error::other).boxed()))
    }

    /// Close the server's stdin, and kill it if it has not exited within [`EXIT_GRACE`].
    pub(super) async fn close(self) {
        let Stdio { driver, child, .. } = self;
        driver.abort();
        let _ = driver.await;
        let Some(mut child) = child else {
            return;
        };
        if tokio::time::timeout(EXIT_GRACE, child.wait())
            .await
            .is_err()
        {
            let _ = child.kill().await;
        }
    }
}
