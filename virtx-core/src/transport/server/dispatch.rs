//! The server's methods, and what one message to them answers.

use std::{collections::HashMap, future::Future, pin::Pin, sync::Arc};

use http::Extensions;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::sync::mpsc;

use crate::method::{Progress, Request, StreamingRequest};
use crate::transport::message::{
    ErrorObject, Notification, PROGRESS, ProgressParams, Received, response,
};

/// Where a method sends the progress of the request it is answering: the request's event
/// stream. In a method's [`Extensions`] for every request, so a method gets it with
/// `extensions.get::<Notifier>()`.
#[derive(Clone, Debug)]
pub struct Notifier {
    sender: mpsc::UnboundedSender<Vec<u8>>,
    /// The `id` of the request being answered; `None` for a notification, which has no
    /// progress to send.
    request_id: Option<Value>,
}

impl Notifier {
    pub(crate) fn new(sender: mpsc::UnboundedSender<Vec<u8>>) -> Notifier {
        Notifier {
            sender,
            request_id: None,
        }
    }

    /// Send `progress` about this request, an `R`, as a [`PROGRESS`] notification. False if
    /// nothing will read it: the message was a notification, or the client took no stream, or
    /// is gone.
    pub fn notify<R: StreamingRequest>(&self, progress: Progress<R::Content>) -> bool {
        let Some(request_id) = self.request_id.clone() else {
            return false;
        };
        let notification = Notification {
            jsonrpc: "2.0",
            method: PROGRESS,
            params: ProgressParams {
                request_id,
                progress: progress.progress,
                total: progress.total,
                content: progress.content,
            },
        };
        let bytes = serde_json::to_vec(&notification).expect("a notification encodes");
        self.sender.send(bytes).is_ok()
    }
}

type Answering = Pin<Box<dyn Future<Output = Result<Value, ErrorObject>> + Send>>;

type Handler = Arc<dyn Fn(Option<Value>, Extensions) -> Answering + Send + Sync>;

/// The methods a server answers, by name.
#[derive(Clone, Default)]
pub struct Methods(HashMap<&'static str, Handler>);

impl Methods {
    pub fn new() -> Methods {
        Methods::default()
    }

    /// Answer `R` with `handler`, given the request's params read as `R::Args` and its
    /// [`Extensions`]. Params that do not read are refused before `handler` runs.
    pub fn add<R, F, Fut>(&mut self, handler: F) -> &mut Methods
    where
        R: Request,
        R::Args: Send + 'static,
        R::Output: Send,
        F: Fn(R::Args, Extensions) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<R::Output, ErrorObject>> + Send + 'static,
    {
        let handler = Arc::new(handler);
        let handler: Handler = Arc::new(move |params, extensions| {
            let handler = Arc::clone(&handler);
            Box::pin(async move {
                let args = read_params::<R::Args>(params)?;
                let output = handler(args, extensions).await?;
                serde_json::to_value(&output)
                    .map_err(|e| ErrorObject::new(ErrorObject::INTERNAL_ERROR, e.to_string()))
            })
        });
        self.0.insert(R::NAME, handler);
        self
    }
}

/// `params` read as `A`; no params read as `{}`.
fn read_params<A: DeserializeOwned>(params: Option<Value>) -> Result<A, ErrorObject> {
    serde_json::from_value(params.unwrap_or_else(|| Value::Object(Default::default())))
        .map_err(|e| ErrorObject::new(ErrorObject::INVALID_PARAMS, e.to_string()))
}

/// The methods a server answers, and what one message to them answers. Cheap to clone.
#[derive(Clone)]
pub struct Dispatcher {
    methods: Arc<Methods>,
}

impl Dispatcher {
    pub fn new(methods: Methods) -> Dispatcher {
        Dispatcher {
            methods: Arc::new(methods),
        }
    }

    /// Hand `message` to its method, with `extensions` the method's to read and their
    /// [`Notifier`] bound to the request, and return the response; `None` for a message that
    /// takes none, a notification or a response.
    pub(crate) async fn call(&self, message: &[u8], mut extensions: Extensions) -> Option<Vec<u8>> {
        let failed = |id: &Value, code, message: &str| {
            Some(response(id, Err(ErrorObject::new(code, message))))
        };
        // One value, and nothing after it.
        let message = match serde_json::from_slice::<Value>(message) {
            Ok(message) => message,
            Err(_) => return failed(&Value::Null, ErrorObject::PARSE_ERROR, "Parse error"),
        };
        // An object, or not a request: a batch is not taken.
        let received = match message {
            Value::Object(_) => serde_json::from_value::<Received>(message).ok(),
            _ => None,
        };
        let Some(received) = received else {
            return failed(
                &Value::Null,
                ErrorObject::INVALID_REQUEST,
                "Invalid request",
            );
        };

        let id = match received.id {
            None => None,
            Some(id @ (Value::Null | Value::String(_))) => Some(id),
            Some(Value::Number(id)) if id.is_i64() || id.is_u64() => Some(Value::Number(id)),
            Some(_) => {
                return failed(
                    &Value::Null,
                    ErrorObject::INVALID_REQUEST,
                    "Invalid request",
                );
            }
        };
        let params_structured = received
            .params
            .as_ref()
            .is_none_or(|params| matches!(params, Value::Object(_) | Value::Array(_)));
        let method = match received.method {
            Some(method) if received.jsonrpc.as_deref() == Some("2.0") && params_structured => {
                method
            }
            // A response: the server sends no requests, so none is awaited.
            None if received.result.is_some() || received.error.is_some() => return None,
            _ => {
                let id = id.unwrap_or(Value::Null);
                return failed(&id, ErrorObject::INVALID_REQUEST, "Invalid request");
            }
        };

        if let Some(notifier) = extensions.get_mut::<Notifier>() {
            notifier.request_id = id.clone();
        }
        let outcome = match self.methods.0.get(method.as_str()) {
            Some(handler) => handler(received.params, extensions).await,
            None => Err(ErrorObject::new(
                ErrorObject::METHOD_NOT_FOUND,
                "Method not found",
            )),
        };
        id.map(|id| response(&id, outcome))
    }
}

#[cfg(all(test, feature = "client"))]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::transport::tests::methods;

    fn decode(bytes: &[u8]) -> Value {
        serde_json::from_slice(bytes).unwrap()
    }

    async fn call(message: Value) -> Option<Value> {
        let answer = Dispatcher::new(methods())
            .call(message.to_string().as_bytes(), Extensions::new())
            .await;
        answer.map(|answer| decode(&answer))
    }

    #[tokio::test]
    async fn a_request_answers_its_response() {
        let request = json!({"jsonrpc": "2.0", "id": 1, "method": "echo", "params": ["hi"]});
        assert_eq!(
            call(request).await,
            Some(json!({"jsonrpc": "2.0", "id": 1, "result": "hi"}))
        );
    }

    #[tokio::test]
    async fn a_notification_answers_nothing() {
        let notification = json!({"jsonrpc": "2.0", "method": "echo", "params": ["hi"]});
        assert_eq!(call(notification).await, None);
    }

    #[tokio::test]
    async fn a_response_answers_nothing() {
        let response = json!({"jsonrpc": "2.0", "id": 1, "result": 2});
        assert_eq!(call(response).await, None);
    }

    /// What `message` answers, given a [`Notifier`], and the notifications sent while it ran.
    async fn call_notified(message: Value) -> (Option<Value>, Vec<Value>) {
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let mut extensions = Extensions::new();
        extensions.insert(Notifier::new(sender));
        let answer = Dispatcher::new(methods())
            .call(message.to_string().as_bytes(), extensions)
            .await;
        let mut sent = Vec::new();
        while let Ok(notification) = receiver.try_recv() {
            sent.push(decode(&notification));
        }
        (answer.map(|answer| decode(&answer)), sent)
    }

    #[tokio::test]
    async fn progress_names_the_request_by_its_id() {
        let request = json!({"jsonrpc": "2.0", "id": "a", "method": "count", "params": [2]});
        let (answer, sent) = call_notified(request).await;
        assert_eq!(
            answer,
            Some(json!({"jsonrpc": "2.0", "id": "a", "result": 2}))
        );
        let progress = |progress: u32| {
            json!({
                "jsonrpc": "2.0",
                "method": "notifications/progress",
                "params": {"requestId": "a", "progress": progress, "total": 2},
            })
        };
        assert_eq!(sent, [progress(1), progress(2)]);
    }

    #[tokio::test]
    async fn a_notification_sends_no_progress() {
        let notification = json!({"jsonrpc": "2.0", "method": "count", "params": [2]});
        let (answer, sent) = call_notified(notification).await;
        assert_eq!(answer, None);
        assert!(sent.is_empty());
    }

    /// The `error.code` `message` is answered with.
    async fn code(message: &[u8]) -> i64 {
        let answer = Dispatcher::new(methods())
            .call(message, Extensions::new())
            .await
            .expect("a response");
        let answer: Received = serde_json::from_slice(&answer).unwrap();
        answer.error.expect("an error").code
    }

    #[tokio::test]
    async fn what_is_not_a_request_answers_the_spec_s_errors() {
        let request = json!({"jsonrpc": "2.0", "id": 1, "method": "echo", "params": ["hi"]});
        let encode = |value: Value| value.to_string().into_bytes();
        let cases = [
            (b"not json".to_vec(), ErrorObject::PARSE_ERROR),
            (Vec::new(), ErrorObject::PARSE_ERROR),
            (
                format!("{request} 0").into_bytes(),
                ErrorObject::PARSE_ERROR,
            ),
            (b"\xff".to_vec(), ErrorObject::PARSE_ERROR),
            (encode(json!([])), ErrorObject::INVALID_REQUEST),
            (encode(json!([request])), ErrorObject::INVALID_REQUEST),
            (
                encode(json!({"jsonrpc": "1.0", "id": 1, "method": "echo"})),
                ErrorObject::INVALID_REQUEST,
            ),
            (
                encode(json!({"jsonrpc": "2.0", "id": 1})),
                ErrorObject::INVALID_REQUEST,
            ),
            (
                encode(json!({"jsonrpc": "2.0", "id": {}, "method": "echo"})),
                ErrorObject::INVALID_REQUEST,
            ),
            (
                encode(json!({"jsonrpc": "2.0", "id": 1.5, "method": "echo"})),
                ErrorObject::INVALID_REQUEST,
            ),
            (
                encode(json!({"jsonrpc": "2.0", "id": 1, "method": "echo", "params": "hi"})),
                ErrorObject::INVALID_REQUEST,
            ),
            (
                encode(json!({"jsonrpc": "2.0", "id": 1, "method": "echo", "params": null})),
                ErrorObject::INVALID_REQUEST,
            ),
            (
                encode(json!({"jsonrpc": "2.0", "id": 1, "method": "nope"})),
                ErrorObject::METHOD_NOT_FOUND,
            ),
            (
                encode(json!({"jsonrpc": "2.0", "id": 1, "method": "echo", "params": [1]})),
                ErrorObject::INVALID_PARAMS,
            ),
        ];
        for (message, expected) in cases {
            let shown = String::from_utf8_lossy(&message).into_owned();
            assert_eq!(code(&message).await, expected, "{shown}");
        }
    }
}
