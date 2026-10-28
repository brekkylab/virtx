//! JSON-RPC 2.0 messages, as this protocol uses them: one JSON object each, never a batch.

// Each end uses only its own share of these.
#![cfg_attr(not(all(feature = "client", feature = "server")), allow(dead_code))]

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

/// The media type of one message.
pub(crate) const JSON: &str = "application/json";

/// The media type of a stream of messages, each one event's `data`.
pub(crate) const EVENT_STREAM: &str = "text/event-stream";

/// Whether `content_type` names `media_type`, whatever its parameters.
pub(crate) fn is_media_type(content_type: &str, media_type: &str) -> bool {
    let name = content_type.split(';').next().unwrap_or("").trim();
    name.eq_ignore_ascii_case(media_type)
}

/// The `error` of a response: why a request failed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ErrorObject {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl ErrorObject {
    /// The body was not one JSON value.
    pub(crate) const PARSE_ERROR: i64 = -32700;
    /// The message was JSON, but not a request.
    pub(crate) const INVALID_REQUEST: i64 = -32600;
    /// No method has the request's name.
    pub(crate) const METHOD_NOT_FOUND: i64 = -32601;
    /// The request's params are not what its method takes.
    pub(crate) const INVALID_PARAMS: i64 = -32602;
    /// The method failed in a way that is the server's, not the request's.
    pub(crate) const INTERNAL_ERROR: i64 = -32603;

    pub fn new(code: i64, message: impl Into<String>) -> ErrorObject {
        ErrorObject {
            code,
            message: message.into(),
            data: None,
        }
    }
}

impl fmt::Display for ErrorObject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.message, self.code)?;
        if let Some(data) = &self.data {
            write!(f, ": {data}")?;
        }
        Ok(())
    }
}

/// A message as read, before it is told apart: a request has `method` and `id`, a
/// notification `method` and no `id`, a response `id` and `result` or `error`.
#[derive(Debug, Deserialize)]
pub(crate) struct Received {
    #[serde(default)]
    pub jsonrpc: Option<String>,
    /// `Some(Value::Null)` for an `"id": null`, which is not the same as no `id`.
    #[serde(default, deserialize_with = "present")]
    pub id: Option<Value>,
    #[serde(default)]
    pub method: Option<String>,
    /// `Some(Value::Null)` for a `"params": null`, which is not params.
    #[serde(default, deserialize_with = "present")]
    pub params: Option<Value>,
    /// `Some` for a `"result": null` too.
    #[serde(default, deserialize_with = "present")]
    pub result: Option<Value>,
    #[serde(default)]
    pub error: Option<ErrorObject>,
}

/// A member that is there, even as `null`.
fn present<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

/// A request, or with no `id` a notification.
#[derive(Serialize)]
pub(crate) struct Request<'a> {
    pub jsonrpc: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<&'a str>,
    pub method: &'a str,
    pub params: &'a Value,
}

/// A notification from the server.
#[derive(Serialize)]
pub(crate) struct Notification<P> {
    pub jsonrpc: &'static str,
    pub method: &'static str,
    pub params: P,
}

/// The notification a server sends a request's progress in.
pub(crate) const PROGRESS: &str = "notifications/progress";

/// The notification a client cancels a request with.
pub(crate) const CANCELLED: &str = "notifications/cancelled";

/// The params of a [`CANCELLED`] notification. Its `reason` is for a person, and not read.
#[derive(Deserialize)]
pub(crate) struct CancelledParams {
    #[serde(rename = "requestId")]
    pub request_id: Value,
}

/// The params of a [`PROGRESS`] notification: a [`method::Progress`](crate::method::Progress)
/// and the request it is about.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ProgressParams<C> {
    /// The `id` of the request this is about, as the client sent it.
    #[serde(rename = "requestId")]
    pub request_id: Value,
    pub progress: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<C>,
}

/// The response to the request `id`: its result, or why it failed.
pub(crate) fn response(id: &Value, outcome: Result<Value, ErrorObject>) -> Vec<u8> {
    #[derive(Serialize)]
    struct Success<'a> {
        jsonrpc: &'static str,
        id: &'a Value,
        result: Value,
    }
    #[derive(Serialize)]
    struct Failure<'a> {
        jsonrpc: &'static str,
        id: &'a Value,
        error: ErrorObject,
    }
    let jsonrpc = "2.0";
    match outcome {
        Ok(result) => serde_json::to_vec(&Success {
            jsonrpc,
            id,
            result,
        }),
        Err(error) => serde_json::to_vec(&Failure { jsonrpc, id, error }),
    }
    .expect("a response encodes")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn received(message: Value) -> Received {
        serde_json::from_value(message).unwrap()
    }

    #[test]
    fn a_null_id_params_and_result_are_told_from_none() {
        let received =
            received(json!({"jsonrpc": "2.0", "id": null, "params": null, "result": null}));
        assert_eq!(received.id, Some(Value::Null));
        assert_eq!(received.params, Some(Value::Null));
        assert_eq!(received.result, Some(Value::Null));
    }

    #[test]
    fn a_missing_id_is_none() {
        let received = received(json!({"jsonrpc": "2.0", "method": "m"}));
        assert_eq!(received.id, None);
        assert!(received.result.is_none());
    }

    #[test]
    fn a_response_keeps_its_members_in_order() {
        let answer = response(&Value::from(1), Ok(Value::from("hi")));
        assert_eq!(answer, br#"{"jsonrpc":"2.0","id":1,"result":"hi"}"#);
        let answer = response(&Value::Null, Err(ErrorObject::new(-1, "no")));
        assert_eq!(
            answer,
            br#"{"jsonrpc":"2.0","id":null,"error":{"code":-1,"message":"no"}}"#
        );
    }
}
