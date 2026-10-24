//! The server's end: choosing between one message and a stream of them.

use std::{
    pin::Pin,
    task::{Context, Poll},
};

use bytes::Bytes;
use http::{HeaderMap, HeaderValue, Method, Request, Response, StatusCode, header};
use http_body::{Body, Frame};
use http_body_util::{BodyExt, Empty, Full, Limited, combinators::UnsyncBoxBody};
use tokio::{
    sync::mpsc,
    task::{JoinError, JoinHandle},
};

use super::{CBOR, CBOR_SEQ, is_media_type};
use crate::transport::{Dispatcher, Notifier, r#impl::frame::frame};

/// The body of every response a [`Server`] answers.
pub type HttpBody = UnsyncBoxBody<Bytes, std::convert::Infallible>;

/// The largest message a server reads.
const MAX_MESSAGE: usize = 10 * 1024 * 1024;

/// The server's end of Streamable HTTP, for whatever HTTP server mounts it at the endpoint:
/// [`Server::handle`] answers one HTTP request. Each `POST` stands alone: no session ties one
/// to another.
///
/// It keeps no policy of its own: who may send a request, and which `Origin`s are let in, is
/// the mounting server's middleware to decide, and what that middleware puts in a request's
/// [`Extensions`](http::Extensions) is what the request's method is given.
pub struct Server {
    dispatcher: Dispatcher,
}

impl Server {
    /// Serve `dispatcher`'s methods.
    pub fn new(dispatcher: Dispatcher) -> Server {
        Server { dispatcher }
    }

    /// Answer one HTTP request to the endpoint.
    pub async fn handle<B>(&self, request: Request<B>) -> Response<HttpBody>
    where
        B: Body<Data = Bytes> + Send + 'static,
        B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
    {
        if request.method() != Method::POST {
            let mut response = status(StatusCode::METHOD_NOT_ALLOWED);
            response
                .headers_mut()
                .insert(header::ALLOW, HeaderValue::from_static("POST"));
            return response;
        }
        let streams = accepts(request.headers(), CBOR_SEQ);
        let (parts, body) = request.into_parts();
        let mut extensions = parts.extensions;
        let message = match Limited::new(body, MAX_MESSAGE).collect().await {
            Ok(body) => body.to_bytes(),
            Err(_) => return status(StatusCode::PAYLOAD_TOO_LARGE),
        };

        let (notifier, notifications) = mpsc::unbounded_channel();
        // Without a stream to send them on, notifications go nowhere: dropping the receiver
        // makes `Notifier::notify` say so.
        let notifications = streams.then_some(notifications);
        extensions.insert(Notifier::new(notifier));
        let dispatcher = self.dispatcher.clone();
        let call = tokio::spawn(async move { dispatcher.call(&message, extensions).await });
        match notifications {
            Some(notifications) => stream(notifications, call).await,
            None => single(call.await),
        }
    }
}

/// One message unless a notification comes before the answer; then a stream of every
/// notification, and the answer last.
async fn stream(
    mut notifications: mpsc::UnboundedReceiver<Vec<u8>>,
    mut call: JoinHandle<Option<Vec<u8>>>,
) -> Response<HttpBody> {
    // Notifications first, so one sent just before the call ended is not lost to a
    // one-message answer.
    let first = tokio::select! {
        biased;
        Some(first) = notifications.recv() => first,
        answer = &mut call => return single(answer),
    };
    let (frames, body) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let send = |message: &[u8]| {
            let _ = frames.send(Bytes::from(frame(message)));
        };
        send(&first);
        // The call has ended when its task has, and every notification it sent is queued by
        // then: drain them, then send the answer.
        loop {
            tokio::select! {
                biased;
                Some(notification) = notifications.recv() => send(&notification),
                answer = &mut call => {
                    while let Ok(notification) = notifications.try_recv() {
                        send(&notification);
                    }
                    if let Ok(Some(answer)) = answer {
                        send(&answer);
                    }
                    break;
                }
            }
        }
    });
    let mut response = Response::new(HttpBody::new(Frames(body)));
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(CBOR_SEQ));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    response
}

/// The methods' answer as one message: `202` when there is none to send, `500` when the call
/// panicked.
fn single(answer: Result<Option<Vec<u8>>, JoinError>) -> Response<HttpBody> {
    let body = match answer {
        Ok(Some(body)) => body,
        Ok(None) => return status(StatusCode::ACCEPTED),
        Err(_) => return status(StatusCode::INTERNAL_SERVER_ERROR),
    };
    let mut response = Response::new(HttpBody::new(Full::new(Bytes::from(body))));
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(CBOR));
    response
}

fn status(status: StatusCode) -> Response<HttpBody> {
    let mut response = Response::new(HttpBody::new(Empty::new()));
    *response.status_mut() = status;
    response
}

/// Whether `headers` accept `media_type`, by name: parameters and `q` are not weighed.
fn accepts(headers: &HeaderMap, media_type: &str) -> bool {
    headers
        .get_all(header::ACCEPT)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(|item| is_media_type(item, media_type))
}

/// A streamed body: the frames sent on a channel, ending when it closes.
struct Frames(mpsc::UnboundedReceiver<Bytes>);

impl Body for Frames {
    type Data = Bytes;
    type Error = std::convert::Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        self.0
            .poll_recv(cx)
            .map(|frame| frame.map(|f| Ok(Frame::data(f))))
    }
}
