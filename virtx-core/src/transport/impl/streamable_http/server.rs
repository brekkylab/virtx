//! The server's end: sessions, and choosing between a JSON answer and an SSE stream.

use std::{
    collections::HashSet,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
};

use bytes::Bytes;
use http::{Extensions, HeaderValue, Method, Request, Response, StatusCode, header};
use http_body::{Body, Frame};
use http_body_util::{BodyExt, Empty, Full, Limited, combinators::UnsyncBoxBody};
use tokio::sync::mpsc;

use super::{EVENT_STREAM, SESSION_HEADER, sse};
use crate::transport::{Dispatcher, Notifier};

/// The body of every response a [`Server`] answers.
pub type HttpBody = UnsyncBoxBody<Bytes, std::convert::Infallible>;

/// The largest message a server reads.
const MAX_MESSAGE: usize = 10 * 1024 * 1024;

/// The session a request belongs to, in its method's [`Extensions`].
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SessionId(pub String);

/// The server's end of Streamable HTTP, for whatever HTTP server mounts it at the endpoint:
/// [`Server::handle`] answers one HTTP request. Cheap to clone.
#[derive(Clone)]
pub struct Server {
    inner: Arc<Inner>,
}

struct Inner {
    dispatcher: Dispatcher,
    sessions: Mutex<HashSet<String>>,
    allowed_origins: Vec<String>,
}

impl Server {
    /// Serve `dispatcher`'s methods, refusing every request that carries an `Origin`.
    pub fn new(dispatcher: Dispatcher) -> Server {
        Server::with_origins(dispatcher, Vec::new())
    }

    /// Serve `dispatcher`'s methods, refusing a request whose `Origin` is not one of
    /// `allowed_origins` (as a browser sends it, e.g. `https://example.com`).
    pub fn with_origins(dispatcher: Dispatcher, allowed_origins: Vec<String>) -> Server {
        Server {
            inner: Arc::new(Inner {
                dispatcher,
                sessions: Mutex::new(HashSet::new()),
                allowed_origins,
            }),
        }
    }

    /// Answer one HTTP request to the endpoint.
    pub async fn handle<B>(&self, request: Request<B>) -> Response<HttpBody>
    where
        B: Body<Data = Bytes> + Send + 'static,
        B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
    {
        if let Some(origin) = request.headers().get(header::ORIGIN) {
            let allowed = origin
                .to_str()
                .is_ok_and(|origin| self.inner.allowed_origins.iter().any(|o| o == origin));
            if !allowed {
                return status(StatusCode::FORBIDDEN);
            }
        }
        let session = match request.headers().get(SESSION_HEADER) {
            None => None,
            Some(id) => match id.to_str() {
                Ok(id) if self.has_session(id) => Some(id.to_owned()),
                _ => return status(StatusCode::NOT_FOUND),
            },
        };
        match *request.method() {
            Method::POST => self.post(request, session).await,
            Method::DELETE => match session {
                Some(id) => {
                    self.inner.sessions.lock().unwrap().remove(&id);
                    status(StatusCode::NO_CONTENT)
                }
                None => status(StatusCode::BAD_REQUEST),
            },
            _ => {
                let mut response = status(StatusCode::METHOD_NOT_ALLOWED);
                response
                    .headers_mut()
                    .insert(header::ALLOW, HeaderValue::from_static("POST, DELETE"));
                response
            }
        }
    }

    fn has_session(&self, id: &str) -> bool {
        self.inner.sessions.lock().unwrap().contains(id)
    }

    /// One message, in `session` or opening one.
    async fn post<B>(&self, request: Request<B>, session: Option<String>) -> Response<HttpBody>
    where
        B: Body<Data = Bytes> + Send + 'static,
        B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
    {
        let streams = accepts(request.headers(), EVENT_STREAM);
        let message = match Limited::new(request.into_body(), MAX_MESSAGE)
            .collect()
            .await
        {
            Ok(body) => body.to_bytes(),
            Err(_) => return status(StatusCode::PAYLOAD_TOO_LARGE),
        };
        let (session, opened) = match session {
            Some(id) => (id, false),
            None => {
                let id = uuid::Uuid::new_v4().to_string();
                self.inner.sessions.lock().unwrap().insert(id.clone());
                (id, true)
            }
        };

        let (notifier, notifications) = mpsc::unbounded_channel();
        // Without a stream to send them on, notifications go nowhere: dropping the receiver
        // makes `Notifier::notify` say so.
        let mut notifications = streams.then_some(notifications);
        let mut extensions = Extensions::new();
        extensions.insert(SessionId(session.clone()));
        extensions.insert(Notifier::new(notifier));
        let dispatcher = self.inner.dispatcher.clone();
        let mut call = tokio::spawn(async move { dispatcher.call(&message, extensions).await });

        // JSON unless a notification comes before the response. Notifications first, so one
        // sent just before the call ended is not lost to a JSON answer.
        let first = match &mut notifications {
            Some(queue) => tokio::select! {
                biased;
                Some(first) = queue.recv() => Ok(first),
                answer = &mut call => Err(answer),
            },
            None => Err((&mut call).await),
        };
        let mut response = match first {
            Err(answer) => match answer {
                Ok(answer) => json(answer),
                Err(_) => status(StatusCode::INTERNAL_SERVER_ERROR),
            },
            Ok(first) => {
                let mut notifications = notifications.expect("a stream was accepted");
                let (events, body) = mpsc::unbounded_channel();
                let _ = events.send(sse::event(&first));
                tokio::spawn(async move {
                    // The call has ended when its task has, and every notification it sent
                    // is queued by then: drain them, then send the response.
                    loop {
                        tokio::select! {
                            biased;
                            Some(notification) = notifications.recv() => {
                                let _ = events.send(sse::event(&notification));
                            }
                            answer = &mut call => {
                                while let Ok(notification) = notifications.try_recv() {
                                    let _ = events.send(sse::event(&notification));
                                }
                                if let Ok(Some(response)) = answer {
                                    let _ = events.send(sse::event(&response));
                                }
                                break;
                            }
                        }
                    }
                });
                let mut response = Response::new(HttpBody::new(Events(body)));
                response
                    .headers_mut()
                    .insert(header::CONTENT_TYPE, HeaderValue::from_static(EVENT_STREAM));
                response
                    .headers_mut()
                    .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
                response
            }
        };
        if opened {
            let id = HeaderValue::from_str(&session).expect("a UUID is visible ASCII");
            response.headers_mut().insert(SESSION_HEADER, id);
        }
        response
    }
}

/// The methods' answer as this transport's: `202` when there is none to send.
fn json(answer: Option<String>) -> Response<HttpBody> {
    let Some(body) = answer else {
        return status(StatusCode::ACCEPTED);
    };
    let mut response = Response::new(HttpBody::new(Full::new(Bytes::from(body))));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
}

fn status(status: StatusCode) -> Response<HttpBody> {
    let mut response = Response::new(HttpBody::new(Empty::new()));
    *response.status_mut() = status;
    response
}

/// Whether `headers` accept `media_type`, by name: parameters and `q` are not weighed.
fn accepts(headers: &http::HeaderMap, media_type: &str) -> bool {
    headers
        .get_all(header::ACCEPT)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(|item| item.split(';').next().unwrap_or("").trim())
        .any(|item| item.eq_ignore_ascii_case(media_type))
}

/// An SSE body: the events sent on a channel, ending when it closes.
struct Events(mpsc::UnboundedReceiver<Bytes>);

impl Body for Events {
    type Data = Bytes;
    type Error = std::convert::Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        self.0
            .poll_recv(cx)
            .map(|event| event.map(|e| Ok(Frame::data(e))))
    }
}
