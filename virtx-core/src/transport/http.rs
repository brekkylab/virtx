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

/// `response`, a refusal, as the [`Error::Status`] it is: its body read for a person, the
/// `{"error": {"code", "message"}}` a resource answers as `code: message`.
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
    let status = response.status().as_u16();
    let body = response
        .into_body()
        .collect()
        .await
        .map(|body| body.to_bytes())
        .unwrap_or_default();
    let message = match serde_json::from_slice::<Refusal>(&body) {
        Ok(Refusal { error }) => format!("{}: {}", error.code, error.message),
        Err(_) => String::from_utf8_lossy(&body).trim().to_owned(),
    };
    Error::Status(status, message)
}

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
}
