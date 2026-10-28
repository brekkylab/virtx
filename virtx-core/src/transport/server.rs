//! The server's end of stdio: one HTTP/2 connection over its own stdin and stdout.

use std::{convert::Infallible, future::Future, io};

use bytes::Bytes;
use http::{Request, Response};
use http_body::Body;
use hyper::{body::Incoming, service::service_fn};
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::io::{AsyncRead, AsyncWrite};

/// Serve one HTTP/2 connection over `input` and `output` (a server's stdin and stdout), each
/// request answered by `handle`, until the client closes it or `input` ends. What is in flight
/// then is cancelled, not waited for.
///
/// Every endpoint is `handle`'s, as a server over HTTP routes its own: [`Server`](crate::Server)
/// answers `/rpc`.
pub async fn serve_stdio<I, O, F, Fut, B>(handle: F, input: I, output: O) -> io::Result<()>
where
    I: AsyncRead + Send + Unpin + 'static,
    O: AsyncWrite + Send + Unpin + 'static,
    F: Fn(Request<Incoming>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Response<B>> + Send + 'static,
    B: Body<Data = Bytes> + Send + 'static,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    let service = service_fn(move |request| {
        let answered = handle(request);
        async move { Ok::<_, Infallible>(answered.await) }
    });
    let io = TokioIo::new(tokio::io::join(input, output));
    let served = hyper::server::conn::http2::Builder::new(TokioExecutor::new())
        .serve_connection(io, service)
        .await;
    match served {
        Ok(()) => Ok(()),
        // The client went without saying goodbye, as it may.
        Err(e) if e.is_incomplete_message() || is_eof(&e) => Ok(()),
        Err(e) => Err(io::Error::other(e)),
    }
}

/// Whether `error` is the input ending.
fn is_eof(error: &hyper::Error) -> bool {
    let mut source = std::error::Error::source(error);
    while let Some(error) = source {
        if let Some(error) = error.downcast_ref::<io::Error>() {
            return matches!(
                error.kind(),
                io::ErrorKind::UnexpectedEof
                    | io::ErrorKind::BrokenPipe
                    | io::ErrorKind::ConnectionReset
            );
        }
        source = error.source();
    }
    false
}
