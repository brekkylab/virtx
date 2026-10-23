//! The server's side of every transport: the methods, and what one message to them answers.

use std::{collections::HashMap, future::Future, pin::Pin, sync::Arc};

use http::Extensions;
use serde::de::DeserializeOwned;
use serde_json::{Value, value::RawValue};
use tokio::sync::mpsc;

use super::message::{ErrorObject, Notification, PROGRESS, Progress, Received, response};
use crate::method::Request;

/// Where a method sends the progress of the request it is answering: the request's SSE stream
/// over HTTP, the server's stdout over stdio. In a method's [`Extensions`] for every request,
/// so a method gets it with `extensions.get::<Notifier>()`.
#[derive(Clone, Debug)]
pub struct Notifier {
    sender: mpsc::UnboundedSender<String>,
    /// The `id` of the request being answered; `None` for a notification, which has no
    /// progress to send.
    request_id: Option<Value>,
}

impl Notifier {
    pub(crate) fn new(sender: mpsc::UnboundedSender<String>) -> Notifier {
        Notifier {
            sender,
            request_id: None,
        }
    }

    /// Send `line`, one line of this request's log, as a [`PROGRESS`] notification. False if
    /// nothing will read it: the message was a notification, or the client took no stream, or
    /// is gone.
    pub fn progress(&self, line: impl Into<String>) -> bool {
        let Some(request_id) = self.request_id.clone() else {
            return false;
        };
        let notification = Notification {
            jsonrpc: "2.0",
            method: PROGRESS,
            params: Progress {
                request_id,
                message: line.into(),
            },
        };
        let notification = serde_json::to_string(&notification).expect("a value serializes");
        self.sender.send(notification).is_ok()
    }
}

type Answering = Pin<Box<dyn Future<Output = Result<Box<RawValue>, ErrorObject>> + Send>>;

type Handler = Arc<dyn Fn(Option<Box<RawValue>>, Extensions) -> Answering + Send + Sync>;

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
                let args = read_params::<R::Args>(params.as_deref())?;
                let output = handler(args, extensions).await?;
                serde_json::value::to_raw_value(&output)
                    .map_err(|e| ErrorObject::new(ErrorObject::INTERNAL_ERROR, e.to_string()))
            })
        });
        self.0.insert(R::NAME, handler);
        self
    }
}

/// `params` read as `A`; no params read as `{}`.
fn read_params<A: DeserializeOwned>(params: Option<&RawValue>) -> Result<A, ErrorObject> {
    let params = params.map_or("{}", RawValue::get);
    serde_json::from_str(params)
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
    pub(crate) async fn call(&self, message: &[u8], mut extensions: Extensions) -> Option<String> {
        let failed = |id: &Value, code, message: &str| {
            Some(response(id, Err(ErrorObject::new(code, message))))
        };
        // An object, or not a request: a batch is not taken.
        let starts_an_object = message.trim_ascii_start().first() == Some(&b'{');
        let received = match serde_json::from_slice::<Received>(message) {
            Ok(received) if starts_an_object => received,
            _ => {
                return match serde_json::from_slice::<serde::de::IgnoredAny>(message) {
                    Ok(_) => failed(
                        &Value::Null,
                        ErrorObject::INVALID_REQUEST,
                        "Invalid request",
                    ),
                    Err(_) => failed(&Value::Null, ErrorObject::PARSE_ERROR, "Parse error"),
                };
            }
        };

        let id = match received.id {
            None => None,
            Some(id @ (Value::Null | Value::String(_) | Value::Number(_))) => Some(id),
            Some(_) => {
                return failed(
                    &Value::Null,
                    ErrorObject::INVALID_REQUEST,
                    "Invalid request",
                );
            }
        };
        let params_structured = received.params.as_ref().is_none_or(|params| {
            matches!(
                params.get().trim_start().as_bytes().first(),
                Some(b'{' | b'[')
            )
        });
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::testing::methods;

    async fn call(message: &str) -> Option<String> {
        Dispatcher::new(methods())
            .call(message.as_bytes(), Extensions::new())
            .await
    }

    #[tokio::test]
    async fn a_request_answers_its_response() {
        let answer = call(r#"{"jsonrpc":"2.0","id":1,"method":"echo","params":["hi"]}"#).await;
        assert_eq!(
            answer.as_deref(),
            Some(r#"{"jsonrpc":"2.0","id":1,"result":"hi"}"#)
        );
    }

    #[tokio::test]
    async fn a_notification_answers_nothing() {
        let answer = call(r#"{"jsonrpc":"2.0","method":"echo","params":["hi"]}"#).await;
        assert_eq!(answer, None);
    }

    #[tokio::test]
    async fn a_response_answers_nothing() {
        assert_eq!(call(r#"{"jsonrpc":"2.0","id":1,"result":2}"#).await, None);
    }

    /// What `message` answers, given a [`Notifier`], and the notifications sent while it ran.
    async fn call_notified(message: &str) -> (Option<String>, Vec<String>) {
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let mut extensions = Extensions::new();
        extensions.insert(Notifier::new(sender));
        let answer = Dispatcher::new(methods())
            .call(message.as_bytes(), extensions)
            .await;
        let mut sent = Vec::new();
        while let Ok(notification) = receiver.try_recv() {
            sent.push(notification);
        }
        (answer, sent)
    }

    #[tokio::test]
    async fn progress_names_the_request_by_its_id() {
        let (answer, sent) =
            call_notified(r#"{"jsonrpc":"2.0","id":"a","method":"count","params":[2]}"#).await;
        assert_eq!(
            answer.as_deref(),
            Some(r#"{"jsonrpc":"2.0","id":"a","result":2}"#)
        );
        assert_eq!(
            sent,
            [
                r#"{"jsonrpc":"2.0","method":"notifications/progress","params":{"requestId":"a","message":"1"}}"#,
                r#"{"jsonrpc":"2.0","method":"notifications/progress","params":{"requestId":"a","message":"2"}}"#,
            ]
        );
    }

    #[tokio::test]
    async fn a_notification_sends_no_progress() {
        let (answer, sent) =
            call_notified(r#"{"jsonrpc":"2.0","method":"count","params":[2]}"#).await;
        assert_eq!(answer, None);
        assert!(sent.is_empty());
    }

    /// The `error.code` `message` is answered with.
    async fn code(message: &str) -> i64 {
        let answer = call(message).await.expect("a response");
        let answer: Value = serde_json::from_str(&answer).unwrap();
        answer["error"]["code"].as_i64().expect("an error")
    }

    #[tokio::test]
    async fn what_is_not_a_request_answers_the_spec_s_errors() {
        let cases = [
            ("not json", ErrorObject::PARSE_ERROR),
            ("[]", ErrorObject::INVALID_REQUEST),
            (
                r#"[{"jsonrpc":"2.0","id":1,"method":"echo","params":["hi"]}]"#,
                ErrorObject::INVALID_REQUEST,
            ),
            (
                r#"{"jsonrpc":"1.0","id":1,"method":"echo"}"#,
                ErrorObject::INVALID_REQUEST,
            ),
            (r#"{"jsonrpc":"2.0","id":1}"#, ErrorObject::INVALID_REQUEST),
            (
                r#"{"jsonrpc":"2.0","id":{},"method":"echo"}"#,
                ErrorObject::INVALID_REQUEST,
            ),
            (
                r#"{"jsonrpc":"2.0","id":1,"method":"echo","params":"hi"}"#,
                ErrorObject::INVALID_REQUEST,
            ),
            (
                r#"{"jsonrpc":"2.0","id":1,"method":"nope"}"#,
                ErrorObject::METHOD_NOT_FOUND,
            ),
            (
                r#"{"jsonrpc":"2.0","id":1,"method":"echo","params":[1]}"#,
                ErrorObject::INVALID_PARAMS,
            ),
        ];
        for (message, expected) in cases {
            assert_eq!(code(message).await, expected, "{message}");
        }
    }
}
