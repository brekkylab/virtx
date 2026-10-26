//! The server: a [`Dispatcher`] of methods, answered by [`Server`] for whatever HTTP server
//! mounts it at `/rpc`, or over stdio by [`serve_stdio`]. One HTTP request is answered with one
//! message, or with an event stream of the request's notifications and then its response.

mod dispatch;
mod stdio;

use std::{
    convert::Infallible,
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

use bytes::Bytes;
use futures_util::StreamExt;
use http::{HeaderMap, HeaderValue, Method, Request, Response, StatusCode, header};
use http_body::Body;
use http_body_util::{BodyExt, Empty, Full, Limited, combinators::UnsyncBoxBody};
use sse_stream::{Sse, SseBody};
use tokio::{
    sync::mpsc,
    task::{JoinError, JoinHandle},
};

pub use self::{
    dispatch::{Dispatcher, Methods, Notifier},
    stdio::serve_stdio,
};
use super::message::{EVENT_STREAM, JSON, is_media_type};

/// The body of every response a [`Server`] answers.
pub type HttpBody = UnsyncBoxBody<Bytes, Infallible>;

/// The largest message a server reads.
const MAX_MESSAGE: usize = 10 * 1024 * 1024;

/// The server's end of `/rpc`, for whatever HTTP server mounts it there: [`Server::handle`]
/// answers one HTTP request. Each `POST` stands alone: no session ties one to another. Cheap to
/// clone.
///
/// It keeps no policy of its own: who may send a request, and which `Origin`s are let in, is
/// the mounting server's middleware to decide, and what that middleware puts in a request's
/// [`Extensions`](http::Extensions) is what the request's method is given.
#[derive(Clone)]
pub struct Server {
    dispatcher: Dispatcher,
}

impl Server {
    /// Serve `dispatcher`'s methods.
    pub fn new(dispatcher: Dispatcher) -> Server {
        Server { dispatcher }
    }

    /// Answer one HTTP request to `/rpc`. Dropping the answer, or its body before it ends,
    /// cancels the request.
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
        let streams = accepts(request.headers(), EVENT_STREAM);
        let (parts, body) = request.into_parts();
        let mut extensions = parts.extensions;
        let message = match Limited::new(body, MAX_MESSAGE).collect().await {
            Ok(body) => body.to_bytes(),
            Err(_) => return status(StatusCode::PAYLOAD_TOO_LARGE),
        };

        let (notifier, mut notifications) = mpsc::unbounded_channel();
        // Without a stream to send them on, notifications go nowhere: closing the receiver
        // makes `Notifier::notify` say so.
        if !streams {
            notifications.close();
        }
        extensions.insert(Notifier::new(notifier));
        let dispatcher = self.dispatcher.clone();
        let mut call = Call(tokio::spawn(async move {
            dispatcher.call(&message, extensions).await
        }));
        // One message unless a notification comes before the answer. Notifications first, so
        // one sent just before the call ended is not lost to a one-message answer.
        let first = tokio::select! {
            biased;
            Some(first) = notifications.recv() => first,
            answer = &mut call => return single(answer),
        };
        // The call has ended when its task has, and every notification it sent is queued by
        // then: drain them, then send the answer. Dropping the body drops `call`, cancelling it.
        let messages = async_stream::stream! {
            yield first;
            loop {
                tokio::select! {
                    biased;
                    Some(notification) = notifications.recv() => yield notification,
                    answer = &mut call => {
                        while let Ok(notification) = notifications.try_recv() {
                            yield notification;
                        }
                        if let Ok(Some(answer)) = answer {
                            yield answer;
                        }
                        break;
                    }
                }
            }
        };
        let events = messages.map(|message| {
            let message = String::from_utf8(message).expect("JSON is UTF-8");
            Ok::<_, Infallible>(Sse::default().data(message))
        });
        let mut response = Response::new(HttpBody::new(SseBody::new(events)));
        let headers = response.headers_mut();
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(EVENT_STREAM));
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
        response
    }
}

/// A request's method running, which dropping cancels.
struct Call(JoinHandle<Option<Vec<u8>>>);

impl Future for Call {
    type Output = Result<Option<Vec<u8>>, JoinError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.0).poll(cx)
    }
}

impl Drop for Call {
    fn drop(&mut self) {
        self.0.abort();
    }
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
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(JSON));
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
