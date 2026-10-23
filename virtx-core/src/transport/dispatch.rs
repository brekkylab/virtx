//! The server's side of every transport: the methods, and what one message to them answers.

use std::{collections::HashMap, future::Future, pin::Pin, sync::Arc};

use http::Extensions;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, value::RawValue};
use tokio::sync::mpsc;

use super::message::{ErrorObject, Received, response};
use crate::method::Request;

/// Where a method sends notifications about the request it is answering: the request's SSE
/// stream over HTTP, the server's stdout over stdio. In a method's [`Extensions`] for every
/// request, so a method gets it with `extensions.get::<Notifier>()`.
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

    /// Hand `message` to its method, with `extensions` the method's to read, and return the
    /// response; `None` for a message that takes none, a notification or a response.
    pub(crate) async fn call(&self, message: &[u8], extensions: Extensions) -> Option<String> {
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
    use crate::transport::testing::{Count, methods};

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

    #[tokio::test]
    async fn a_method_reaches_the_notifier_it_was_given() {
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let mut extensions = Extensions::new();
        extensions.insert(Notifier::new(sender));
        let mut methods = Methods::new();
        methods.add::<Count, _, _>(|(to,), extensions| async move {
            let notifier = extensions.get::<Notifier>().expect("a notifier");
            notifier.notify("progress", to);
            Ok(to)
        });
        let answer = Dispatcher::new(methods)
            .call(
                br#"{"jsonrpc":"2.0","id":1,"method":"count","params":[1]}"#,
                extensions,
            )
            .await;
        assert_eq!(
            answer.as_deref(),
            Some(r#"{"jsonrpc":"2.0","id":1,"result":1}"#)
        );
        assert_eq!(
            receiver.recv().await.as_deref(),
            Some(r#"{"jsonrpc":"2.0","method":"progress","params":1}"#)
        );
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
