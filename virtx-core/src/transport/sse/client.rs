//! The client's end of a job's log: its event stream, read as entries.

use futures_util::{Stream, StreamExt};
use http::header;
use serde::de::DeserializeOwned;
use sse_stream::SseStream;

use super::LogEntry;
use crate::{
    Error,
    transport::{TransportClient, full, http::refused},
};

/// The entries of the log at `path`, each read as an `E`, after the `after`th if given: as
/// many as the server sends, until it ends the stream after the job's last.
pub(crate) fn log<'a, E: DeserializeOwned + Send + 'a>(
    client: &'a TransportClient,
    path: String,
    after: Option<u64>,
) -> impl Stream<Item = Result<LogEntry<E>, Error>> + Send + use<'a, E> {
    async_stream::try_stream! {
        let mut request = http::Request::get(&path).header(header::ACCEPT, "text/event-stream");
        if let Some(after) = after {
            request = request.header("Last-Event-ID", after.to_string());
        }
        let request = request
            .body(full(Vec::new()))
            .map_err(|e| Error::Transport(format!("not a request: {e}")))?;
        let response = client.send(request).await?;
        if !response.status().is_success() {
            Err::<(), _>(refused(response).await)?;
            return;
        }
        let mut entries = std::pin::pin!(entries(response, path));
        while let Some(entry) = entries.next().await {
            yield entry?;
        }
    }
}

/// The entries of `response`, a job's log answered from `path`, each read as an `E`.
pub(crate) fn entries<E: DeserializeOwned + Send>(
    response: http::Response<crate::transport::Body>,
    path: String,
) -> impl Stream<Item = Result<LogEntry<E>, Error>> + Send {
    async_stream::try_stream! {
        let mut events = SseStream::new(response.into_body());
        while let Some(event) = events.next().await {
            let event = event.map_err(|e| Error::Transport(format!("reading the log: {e}")))?;
            let Some(data) = event.data else {
                continue;
            };
            let id = event
                .id
                .and_then(|id| id.parse().ok())
                .ok_or_else(|| Error::Transport(format!("an entry of {path} without its id")))?;
            let entry = serde_json::from_str(&data).map_err(|e| Error::Json(e.to_string()))?;
            yield LogEntry { id, entry };
        }
    }
}
