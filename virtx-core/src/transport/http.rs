//! What every endpoint's client shares over HTTP: a path segment percent-encoded, a refusal
//! read as an [`Error`], and a JSON request and its answer, as `PROTOCOL.md`'s Resources says.

use bytes::Bytes;
use http::{Method, header};
use http_body_util::BodyExt;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use super::{Body, TransportClient, full};
use crate::Error;

/// What a path segment keeps as it is: the unreserved characters of RFC 3986.
const SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// `segment`, percent-encoded as one path segment.
pub(crate) fn segment(segment: &str) -> String {
    utf8_percent_encode(segment, SEGMENT).to_string()
}

/// `path`, `/`-separated, each segment percent-encoded, empty segments dropped.
pub(crate) fn segments(path: &str) -> String {
    path.split('/')
        .filter(|s| !s.is_empty())
        .map(segment)
        .collect::<Vec<_>>()
        .join("/")
}

/// `response`, a refusal, as the [`Error::Status`] or [`Error::Refusal`] it is: its body read
/// for a person -- the `{"error": {"code", "message"}}` a resource answers as `code: message`,
/// plain text as it is -- or what any other refusal was rather than its body.
pub(crate) async fn refused(response: http::Response<Body>) -> Error {
    #[derive(Deserialize)]
    struct Refusal {
        error: Reason,
    }
    #[derive(Deserialize)]
    struct Reason {
        code: String,
        message: String,
    }
    let (parts, body) = response.into_parts();
    let status = parts.status.as_u16();
    let content_type = parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .map(str::trim)
        .unwrap_or("");
    let body = body
        .collect()
        .await
        .map(|body| body.to_bytes())
        .unwrap_or_default();
    if body.is_empty() {
        return Error::Status(status, String::new());
    }
    match serde_json::from_slice::<Refusal>(&body) {
        Ok(Refusal { error }) => {
            Error::Status(status, format!("{}: {}", error.code, error.message))
        }
        // Words, as a server or a proxy in front of it may answer: said as they are, as the
        // files client says them. A page -- a proxy's HTML -- is not words to a person.
        Err(_) if content_type == "text/plain" && std::str::from_utf8(&body).is_ok() => {
            let said = String::from_utf8_lossy(&body);
            Error::Status(status, said.trim().chars().take(TEXT_MOST).collect())
        }
        Err(_) => {
            let desc = if content_type.is_empty() {
                format!("{} bytes", body.len())
            } else {
                format!("{content_type}, {} bytes", body.len())
            };
            let mut description = format!("not a virtx answer ({desc})");
            if matches!(status, 502..=504) {
                description.push_str(": the service is unreachable or restarting");
            }
            let raw_body = String::from_utf8_lossy(&body).into_owned();
            Error::Refusal {
                status,
                description,
                body: Some(raw_body),
            }
        }
    }
}

/// The most of a plain-text refusal said as its message.
const TEXT_MOST: usize = 1000;

/// Send `method` to `path` with `body` as JSON, if any, and answer the body of a success. A
/// status outside `2xx` is an [`Error::Status`].
async fn send(
    client: &TransportClient,
    method: Method,
    path: &str,
    body: Option<&impl Serialize>,
) -> Result<Bytes, Error> {
    let mut request = http::Request::builder().method(method).uri(path);
    let body = match body {
        Some(body) => {
            request = request.header(header::CONTENT_TYPE, "application/json");
            serde_json::to_vec(body).map_err(|e| Error::Json(e.to_string()))?
        }
        None => Vec::new(),
    };
    let request = request
        .header(header::ACCEPT, "application/json")
        .body(full(body))
        .map_err(|e| Error::Transport(format!("not a request: {e}")))?;
    let response = client.send(request).await?;
    if !response.status().is_success() {
        return Err(refused(response).await);
    }
    response
        .into_body()
        .collect()
        .await
        .map(|body| body.to_bytes())
        .map_err(|e| Error::Transport(format!("reading the response: {e}")))
}

/// The body of `response`, a success, read as a `T`.
pub(crate) async fn json<T: DeserializeOwned>(
    response: ::http::Response<Body>,
) -> Result<T, Error> {
    let bytes = response
        .into_body()
        .collect()
        .await
        .map(|body| body.to_bytes())
        .map_err(|e| Error::Transport(format!("reading the response: {e}")))?;
    read(&bytes)
}

/// `bytes` read as a `T`.
fn read<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, Error> {
    serde_json::from_slice(bytes).map_err(|e| Error::Json(e.to_string()))
}

/// What `GET path` answers.
pub(crate) async fn get<T: DeserializeOwned>(
    client: &TransportClient,
    path: &str,
) -> Result<T, Error> {
    read(&send(client, Method::GET, path, None::<&()>).await?)
}

/// What `POST path` with `body` answers.
pub(crate) async fn post<T: DeserializeOwned>(
    client: &TransportClient,
    path: &str,
    body: &impl Serialize,
) -> Result<T, Error> {
    read(&send(client, Method::POST, path, Some(body)).await?)
}

/// `POST path` with `body`, answered with nothing.
pub(crate) async fn post_empty(
    client: &TransportClient,
    path: &str,
    body: &impl Serialize,
) -> Result<(), Error> {
    send(client, Method::POST, path, Some(body)).await.map(drop)
}

/// What `PUT path` with `body` answers.
pub(crate) async fn put<T: DeserializeOwned>(
    client: &TransportClient,
    path: &str,
    body: &impl Serialize,
) -> Result<T, Error> {
    read(&send(client, Method::PUT, path, Some(body)).await?)
}

/// `DELETE path`, answered with nothing.
pub(crate) async fn delete(client: &TransportClient, path: &str) -> Result<(), Error> {
    send(client, Method::DELETE, path, None::<&()>)
        .await
        .map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_s_segments_are_percent_encoded() {
        assert_eq!(segment("my vol"), "my%20vol");
        assert_eq!(segment("a/b"), "a%2Fb");
        assert_eq!(segments("/dir//a b.txt/"), "dir/a%20b.txt");
        assert_eq!(segments("한글/#?"), "%ED%95%9C%EA%B8%80/%23%3F");
        assert_eq!(segments(""), "");
    }

    #[tokio::test]
    async fn a_non_json_refusal_says_what_it_was_without_quoting_it() {
        let response = http::Response::builder()
            .status(503)
            .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
            .body(full(Bytes::from(
                "<html><title>503 Service Unavailable</title></html>",
            )))
            .unwrap();
        let error = refused(response).await;
        assert_eq!(
            error.to_string(),
            "the server answered 503 Service Unavailable; not a virtx answer (text/html, 51 bytes): the service is unreachable or restarting"
        );
        let Error::Refusal { status, body, .. } = &error else {
            panic!("expected Error::Refusal, got {error:?}");
        };
        assert_eq!(*status, 503);
        assert_eq!(
            body.as_deref(),
            Some("<html><title>503 Service Unavailable</title></html>")
        );
    }

    #[tokio::test]
    async fn a_non_json_refusal_without_content_type() {
        let response = http::Response::builder()
            .status(502)
            .body(full(Bytes::from("bad gateway")))
            .unwrap();
        let error = refused(response).await;
        assert_eq!(
            error.to_string(),
            "the server answered 502 Bad Gateway; not a virtx answer (11 bytes): the service is unreachable or restarting"
        );
    }

    /// Plain text is said as it is, a `503` and a proxy's alike, as words to a person -- not as
    /// "not a virtx answer" or "unreachable or restarting", of a server that answered (issue
    /// #164).
    #[tokio::test]
    async fn a_plain_text_refusal_is_said_as_it_is() {
        for (status, text, said) in [
            (
                500,
                "internal crash\n",
                "the server answered 500 Internal Server Error: internal crash",
            ),
            (
                503,
                "no node has room for the sandbox yet: the cell is growing",
                "the server answered 503 Service Unavailable: no node has room for the sandbox yet: the cell is growing",
            ),
        ] {
            let response = http::Response::builder()
                .status(status)
                .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
                .body(full(Bytes::from(text)))
                .unwrap();
            let error = refused(response).await;
            assert_eq!(error.to_string(), said);
            assert!(
                matches!(error, Error::Status(s, _) if s == status),
                "{error:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_protocol_refusal_keeps_its_code_and_message() {
        let response = http::Response::builder()
            .status(422)
            .header(header::CONTENT_TYPE, "application/json")
            .body(full(Bytes::from(
                r#"{"error": {"code": "not_started", "message": "no such binary"}}"#,
            )))
            .unwrap();
        let error = refused(response).await;
        assert_eq!(
            error.to_string(),
            "the server answered 422 Unprocessable Entity: not_started: no such binary"
        );
    }

    #[tokio::test]
    async fn an_empty_refusal_says_only_the_status() {
        let response = http::Response::builder()
            .status(401)
            .body(full(Bytes::new()))
            .unwrap();
        let error = refused(response).await;
        assert_eq!(error.to_string(), "the server answered 401 Unauthorized");
    }
}
