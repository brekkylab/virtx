//! The client's end of `/rpc`: a JSON-RPC request as its own `POST`, answered on that `POST`.
//! It knows no method; a typed client is built on it.

use std::pin::pin;

use futures_util::{Stream, StreamExt, TryStreamExt};
use http::{StatusCode, header};
use http_body_util::BodyExt;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use sse_stream::SseStream;

use super::message::{
    EVENT_STREAM, JSON, PROGRESS, ProgressParams, Received, Request, is_media_type,
};
use crate::{
    Error,
    method::{Event, Progress},
    transport::{Body, TransportClient, full},
};

/// Each request is a `POST` of its own: nothing is shared between requests but the connection,
/// and dropping a request's stream closes its `POST`, which the server takes as its
/// cancellation.
impl TransportClient {
    /// Call `method` with `params`, a value that serializes to a map or an array, and
    /// wait for its result. Its progress is not read.
    pub async fn request<R: DeserializeOwned + Send>(
        &self,
        method: &str,
        params: impl Serialize,
    ) -> Result<R, Error> {
        let events = self.request_with_progress::<R, Value>(method, params);
        let mut events = pin!(events);
        while let Some(event) = events.next().await {
            if let Event::Output(output) = event? {
                return Ok(output);
            }
        }
        unreachable!("a call's events end with its output or its error")
    }

    /// Call `method` with `params`, as in [`request`](Self::request): the progress the server
    /// sends about it, its `content` read as `C`, then its result last, or the error it failed
    /// with. Dropping the stream cancels the call.
    pub fn request_with_progress<'a, R, C>(
        &'a self,
        method: &'a str,
        params: impl Serialize,
    ) -> impl Stream<Item = Result<Event<R, C>, Error>> + Send + 'a
    where
        R: DeserializeOwned + Send + 'a,
        C: DeserializeOwned + Send + 'a,
    {
        let params = serde_json::to_value(&params).map_err(|e| Error::Json(e.to_string()));
        async_stream::try_stream! {
            let id = uuid::Uuid::new_v4().to_string();
            let request = Request {
                jsonrpc: "2.0",
                id: Some(&id),
                method,
                params: &params?,
            };
            let message = serde_json::to_vec(&request).map_err(|e| Error::Json(e.to_string()))?;
            let request = http::Request::post("/rpc")
                .header(header::CONTENT_TYPE, JSON)
                .header(header::ACCEPT, format!("{JSON}, {EVENT_STREAM}"))
                .body(full(message))
                .expect("a valid request");
            let response = self.send(request).await?;
            let status = response.status();
            let (parts, body) = response.into_parts();
            let body = if status == StatusCode::OK {
                body
            } else {
                let body = body.collect().await.map(|body| body.to_bytes()).unwrap_or_default();
                let body = String::from_utf8_lossy(&body);
                Err::<Body, _>(Error::Transport(
                    format!("the server answered {status}: {body}").trim_end().to_owned(),
                ))?
            };
            let content_type = parts
                .headers
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .unwrap_or("");

            let answer = if is_media_type(content_type, EVENT_STREAM) {
                let mut messages = pin!(events(body));
                loop {
                    let message = messages.try_next().await?.ok_or_else(|| {
                        Error::Transport("the stream ended before the response".into())
                    })?;
                    match message.method.as_deref() {
                        None if message.id.is_some() => break message,
                        Some(PROGRESS) => {
                            if let Some(progress) = progress(message.params, &id) {
                                yield Event::Progress(content(progress)?);
                            }
                        }
                        // Another notification: this protocol has none.
                        _ => {}
                    }
                }
            } else if is_media_type(content_type, JSON) {
                let body = body
                    .collect()
                    .await
                    .map_err(|e| Error::Transport(format!("reading the response: {e}")))?;
                serde_json::from_slice::<Received>(&body.to_bytes())
                    .map_err(|e| Error::Transport(format!("not a response: {e}")))?
            } else {
                Err::<Received, _>(Error::Transport(format!(
                    "an answer of type `{content_type}`"
                )))?
            };
            let result = match (answer.result, answer.error) {
                (_, Some(error)) => Err(Error::Call(error)),
                (Some(result), None) => Ok(result),
                (None, None) => Err(Error::Transport(
                    "a response with neither result nor error".into(),
                )),
            }?;
            yield Event::Output(serde_json::from_value(result).map_err(|e| Error::Json(e.to_string()))?);
        }
    }
}

/// The messages of an event stream, each an event's `data`. An event without `data` is
/// skipped, and so is one whose `data` is not a message.
fn events(body: Body) -> impl Stream<Item = Result<Received, Error>> {
    SseStream::new(body).filter_map(|event| async move {
        match event {
            Ok(event) => serde_json::from_str(&event.data?).ok().map(Ok),
            Err(e) => Some(Err(Error::Transport(format!("reading the response: {e}")))),
        }
    })
}

/// The progress `params` carry, if they are about the request `id`.
fn progress(params: Option<Value>, id: &str) -> Option<Progress<Value>> {
    let params: ProgressParams<Value> = serde_json::from_value(params?).ok()?;
    (params.request_id == id).then_some(Progress {
        progress: params.progress,
        total: params.total,
        content: params.content,
    })
}

/// `progress` with its `content` read as `C`.
fn content<C: DeserializeOwned>(progress: Progress<Value>) -> Result<Progress<C>, Error> {
    let content = progress
        .content
        .map(serde_json::from_value)
        .transpose()
        .map_err(|e| Error::Json(e.to_string()))?;
    Ok(Progress {
        progress: progress.progress,
        total: progress.total,
        content,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn an_event_is_tagged_by_what_it_is() {
        let cases = [
            (
                Event::Progress(Progress {
                    progress: 1,
                    total: None,
                    content: Some("pulling".into()),
                }),
                json!({"progress": {"progress": 1, "content": "pulling"}}),
            ),
            (
                Event::Progress(Progress {
                    progress: 1,
                    total: Some(3),
                    content: None,
                }),
                json!({"progress": {"progress": 1, "total": 3}}),
            ),
            (Event::Output(2), json!({"output": 2})),
        ];
        for (event, value) in cases {
            assert_eq!(serde_json::to_value(&event).unwrap(), value);
            assert_eq!(
                serde_json::from_value::<Event<u32, String>>(value).unwrap(),
                event
            );
        }
    }
}
