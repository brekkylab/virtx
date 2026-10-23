//! The server's end: sessions, and choosing between one message and a stream of them.

use std::{
    collections::HashMap,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
};

use bytes::Bytes;
use http::{Extensions, HeaderMap, HeaderValue, Method, Request, Response, StatusCode, header};
use http_body::{Body, Frame};
use http_body_util::{BodyExt, Empty, Full, Limited, combinators::UnsyncBoxBody};
use tokio::sync::mpsc;

use super::{CBOR, CBOR_SEQ, SESSION_HEADER, is_media_type};
use crate::transport::{
    Dispatcher, Notifier,
    r#impl::{BoxFuture, frame::frame},
};

/// The body of every response a [`Server`] answers.
pub type HttpBody = UnsyncBoxBody<Bytes, std::convert::Infallible>;

/// The largest message a server reads.
const MAX_MESSAGE: usize = 10 * 1024 * 1024;

/// The session a request belongs to, in its method's [`Extensions`].
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SessionId(pub String);

/// Who may open a session, who may use one, and what its methods are given: a [`Server`]
/// asks this at each point of a session's life, and keeps no policy of its own.
pub trait Sessions: Send + Sync + 'static {
    /// A request without a session header, from `headers`. `Ok` opens a session whose every
    /// request's [`Extensions`] start from those returned; `Err` answers that status, and
    /// opens none.
    fn open<'a>(&'a self, headers: &'a HeaderMap) -> BoxFuture<'a, Result<Extensions, StatusCode>>;

    /// A request in an open session, before it is served or ends the session. `Err` answers
    /// that status, and leaves the session as it was.
    fn check(&self, session: &Extensions, headers: &HeaderMap) -> Result<(), StatusCode> {
        let _ = (session, headers);
        Ok(())
    }

    /// The session `id` was ended by its client.
    fn close(&self, id: &SessionId, session: Extensions) {
        let _ = (id, session);
    }
}

/// Shared with whatever else holds it, such as a server that closes what the sessions left as
/// it ends.
impl<S: Sessions> Sessions for Arc<S> {
    fn open<'a>(&'a self, headers: &'a HeaderMap) -> BoxFuture<'a, Result<Extensions, StatusCode>> {
        (**self).open(headers)
    }

    fn check(&self, session: &Extensions, headers: &HeaderMap) -> Result<(), StatusCode> {
        (**self).check(session, headers)
    }

    fn close(&self, id: &SessionId, session: Extensions) {
        (**self).close(id, session)
    }
}

/// Every request opens a session, and every session is anyone's.
struct Open;

impl Sessions for Open {
    fn open<'a>(&'a self, _: &'a HeaderMap) -> BoxFuture<'a, Result<Extensions, StatusCode>> {
        Box::pin(async { Ok(Extensions::new()) })
    }
}

/// The server's end of Streamable HTTP, for whatever HTTP server mounts it at the endpoint:
/// [`Server::handle`] answers one HTTP request. Cheap to clone.
#[derive(Clone)]
pub struct Server {
    inner: Arc<Inner>,
}

struct Inner {
    dispatcher: Dispatcher,
    /// Each open session's id, with the extensions [`Sessions::open`] gave it.
    sessions: Mutex<HashMap<String, Extensions>>,
    policy: Box<dyn Sessions>,
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
        Server::with_sessions(dispatcher, allowed_origins, Open)
    }

    /// As [`Server::with_origins`], opening, using and ending sessions as `sessions` allows.
    pub fn with_sessions(
        dispatcher: Dispatcher,
        allowed_origins: Vec<String>,
        sessions: impl Sessions,
    ) -> Server {
        Server {
            inner: Arc::new(Inner {
                dispatcher,
                sessions: Mutex::new(HashMap::new()),
                policy: Box::new(sessions),
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
            Some(id) => match id.to_str().ok().and_then(|id| self.session(id)) {
                Some(session) => Some(session),
                None => return status(StatusCode::NOT_FOUND),
            },
        };
        if let Some((_, extensions)) = &session
            && let Err(refused) = self.inner.policy.check(extensions, request.headers())
        {
            return status(refused);
        }
        match *request.method() {
            Method::POST => match session {
                Some((id, extensions)) => self.post(request, Some(id), extensions).await,
                // Asked before the body is read, so a refused client sends no more than headers.
                None => match self.inner.policy.open(request.headers()).await {
                    Ok(extensions) => self.post(request, None, extensions).await,
                    Err(refused) => status(refused),
                },
            },
            Method::DELETE => match session {
                // Two `DELETE`s at once: the one that removes it ends it.
                Some((id, _)) => match self.inner.sessions.lock().unwrap().remove(&id) {
                    Some(extensions) => {
                        self.inner.policy.close(&SessionId(id), extensions);
                        status(StatusCode::NO_CONTENT)
                    }
                    None => status(StatusCode::NOT_FOUND),
                },
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

    /// The open session `id`, with its extensions.
    fn session(&self, id: &str) -> Option<(String, Extensions)> {
        let sessions = self.inner.sessions.lock().unwrap();
        Some((id.to_owned(), sessions.get(id)?.clone()))
    }

    /// One message, in `session` or opening one with `extensions`, which are the session's.
    async fn post<B>(
        &self,
        request: Request<B>,
        session: Option<String>,
        mut extensions: Extensions,
    ) -> Response<HttpBody>
    where
        B: Body<Data = Bytes> + Send + 'static,
        B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
    {
        let streams = accepts(request.headers(), CBOR_SEQ);
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
                let mut sessions = self.inner.sessions.lock().unwrap();
                sessions.insert(id.clone(), extensions.clone());
                (id, true)
            }
        };

        let (notifier, notifications) = mpsc::unbounded_channel();
        // Without a stream to send them on, notifications go nowhere: dropping the receiver
        // makes `Notifier::notify` say so.
        let mut notifications = streams.then_some(notifications);
        extensions.insert(SessionId(session.clone()));
        extensions.insert(Notifier::new(notifier));
        let dispatcher = self.inner.dispatcher.clone();
        let mut call = tokio::spawn(async move { dispatcher.call(&message, extensions).await });

        // One message unless a notification comes before the response. Notifications first, so
        // one sent just before the call ended is not lost to a one-message answer.
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
                Ok(answer) => single(answer),
                Err(_) => status(StatusCode::INTERNAL_SERVER_ERROR),
            },
            Ok(first) => {
                let mut notifications = notifications.expect("a stream was accepted");
                let (frames, body) = mpsc::unbounded_channel();
                let _ = frames.send(Bytes::from(frame(&first)));
                tokio::spawn(async move {
                    // The call has ended when its task has, and every notification it sent
                    // is queued by then: drain them, then send the response.
                    loop {
                        tokio::select! {
                            biased;
                            Some(notification) = notifications.recv() => {
                                let _ = frames.send(Bytes::from(frame(&notification)));
                            }
                            answer = &mut call => {
                                while let Ok(notification) = notifications.try_recv() {
                                    let _ = frames.send(Bytes::from(frame(&notification)));
                                }
                                if let Ok(Some(response)) = answer {
                                    let _ = frames.send(Bytes::from(frame(&response)));
                                }
                                break;
                            }
                        }
                    }
                });
                let mut response = Response::new(HttpBody::new(Frames(body)));
                response
                    .headers_mut()
                    .insert(header::CONTENT_TYPE, HeaderValue::from_static(CBOR_SEQ));
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
fn single(answer: Option<Vec<u8>>) -> Response<HttpBody> {
    let Some(body) = answer else {
        return status(StatusCode::ACCEPTED);
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
fn accepts(headers: &http::HeaderMap, media_type: &str) -> bool {
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
