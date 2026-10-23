//! The server's side of every transport: the methods, and what one message to them answers.

use std::sync::{
    Arc,
    atomic::{AtomicU32, Ordering},
};

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use jsonrpsee::{
    Extensions, Methods,
    core::middleware::RpcServiceBuilder,
    server::{
        ConnectionState, ServerConfig, ServerHandle, StopHandle, http::call_with_service_builder,
        stop_channel,
    },
};
use serde::Serialize;
use tokio::sync::{Semaphore, mpsc};

/// Where a method sends notifications about the request it is answering: the request's SSE
/// stream over HTTP, the server's stdout over stdio. In a method's
/// [`Extensions`](jsonrpsee::Extensions) for every request, so a method gets it with
/// `extensions.get::<Notifier>()`.
#[derive(Clone, Debug)]
pub struct Notifier(mpsc::UnboundedSender<String>);

impl Notifier {
    pub(crate) fn new(sender: mpsc::UnboundedSender<String>) -> Notifier {
        Notifier(sender)
    }

    /// Send the notification `method` with `params`. False if nothing will read it: the
    /// client did not ask for a stream, or is gone.
    pub fn notify(&self, method: &str, params: impl Serialize) -> bool {
        let notification = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        self.0.send(notification.to_string()).is_ok()
    }
}

/// The methods a server answers, and how one message is handed to them.
///
/// jsonrpsee's own HTTP handling does the JSON-RPC part (parsing, dispatch, the error
/// objects); a transport only carries a message in and the answer out. Cheap to clone.
#[derive(Clone)]
pub struct Dispatcher {
    inner: Arc<Inner>,
}

struct Inner {
    methods: Methods,
    config: ServerConfig,
    stop: StopHandle,
    /// Holds the server open: dropping it stops `stop`, and every call with it.
    _running: ServerHandle,
    permits: Arc<Semaphore>,
    next_connection: AtomicU32,
}

impl Dispatcher {
    pub fn new(methods: impl Into<Methods>) -> Dispatcher {
        let (stop, running) = stop_channel();
        Dispatcher {
            inner: Arc::new(Inner {
                methods: methods.into(),
                config: ServerConfig::default(),
                stop,
                _running: running,
                permits: Arc::new(Semaphore::new(Semaphore::MAX_PERMITS)),
                next_connection: AtomicU32::new(0),
            }),
        }
    }

    /// Hand `message` to the methods, with `extensions` theirs to read, and return what
    /// they answer.
    pub(crate) async fn call(&self, message: Bytes, extensions: Extensions) -> Answer {
        let inner = &self.inner;
        let mut request = http::Request::post("/")
            .header(http::header::CONTENT_TYPE, "application/json")
            .body(Full::new(message))
            .expect("a valid request");
        request.extensions_mut().extend(extensions);
        let permit = Arc::clone(&inner.permits)
            .acquire_owned()
            .await
            .expect("never closed");
        let id = inner.next_connection.fetch_add(1, Ordering::Relaxed);
        let connection = ConnectionState::new(inner.stop.clone(), id, permit);
        let answer = call_with_service_builder(
            request,
            inner.config.clone(),
            connection,
            inner.methods.clone(),
            RpcServiceBuilder::new(),
        )
        .await;
        let status = answer.status();
        let body = match answer.into_body().collect().await {
            Ok(body) => body.to_bytes(),
            Err(_) => {
                return Answer::Refused(http::StatusCode::INTERNAL_SERVER_ERROR, Bytes::new());
            }
        };
        match status {
            // jsonrpsee's answer to a notification is `null`.
            http::StatusCode::OK if body.is_empty() || &body[..] == b"null" => Answer::Nothing,
            http::StatusCode::OK => Answer::Response(body),
            status => Answer::Refused(status, body),
        }
    }
}

/// What the methods answer one message with.
#[derive(Debug, PartialEq)]
pub(crate) enum Answer {
    /// A response, as JSON.
    Response(Bytes),
    /// Nothing: the message was a notification.
    Nothing,
    /// The message was not taken (too large, say): the HTTP status saying why, and text.
    Refused(http::StatusCode, Bytes),
}

#[cfg(test)]
mod tests {
    use jsonrpsee::RpcModule;

    use super::*;

    fn dispatcher() -> Dispatcher {
        let mut module = RpcModule::new(());
        module
            .register_method("echo", |params, _, _| params.one::<String>())
            .unwrap();
        module
            .register_method("notify", |_, _, extensions| {
                let notifier = extensions.get::<Notifier>().expect("a notifier");
                notifier.notify("progress", 1);
                "done"
            })
            .unwrap();
        Dispatcher::new(module)
    }

    #[tokio::test]
    async fn a_request_answers_its_response() {
        let answer = dispatcher()
            .call(
                Bytes::from(r#"{"jsonrpc":"2.0","id":1,"method":"echo","params":["hi"]}"#),
                Extensions::new(),
            )
            .await;
        assert_eq!(
            answer,
            Answer::Response(Bytes::from(r#"{"jsonrpc":"2.0","id":1,"result":"hi"}"#))
        );
    }

    #[tokio::test]
    async fn a_notification_answers_nothing() {
        let answer = dispatcher()
            .call(
                Bytes::from(r#"{"jsonrpc":"2.0","method":"echo","params":["hi"]}"#),
                Extensions::new(),
            )
            .await;
        assert_eq!(answer, Answer::Nothing);
    }

    #[tokio::test]
    async fn a_method_reaches_the_notifier_it_was_given() {
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let mut extensions = Extensions::new();
        extensions.insert(Notifier::new(sender));
        let answer = dispatcher()
            .call(
                Bytes::from(r#"{"jsonrpc":"2.0","id":1,"method":"notify"}"#),
                extensions,
            )
            .await;
        assert_eq!(
            answer,
            Answer::Response(Bytes::from(r#"{"jsonrpc":"2.0","id":1,"result":"done"}"#))
        );
        assert_eq!(
            receiver.recv().await.as_deref(),
            Some(r#"{"jsonrpc":"2.0","method":"progress","params":1}"#)
        );
    }
}
