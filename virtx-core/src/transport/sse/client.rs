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

/// How many times in a row a log whose connection broke is asked for again, after what was read
/// of it, before the break is the caller's: a server's process replaced, or the connection to
/// it lost, leaves the job running, and its log is all still there.
const RESUMES: u32 = 5;

/// The entries of the log at `path`, each read as an `E`, after the `after`th if given: as
/// many as the server sends, until it ends the stream after the job's last. A connection that
/// breaks on the way is opened again after the last entry read ([`resumed`]).
pub(crate) fn log<'a, E: DeserializeOwned + Send + 'a>(
    client: &'a TransportClient,
    path: String,
    after: Option<u64>,
) -> impl Stream<Item = Result<LogEntry<E>, Error>> + Send + use<'a, E> {
    async_stream::try_stream! {
        let response = open(client, &path, after).await?;
        let mut entries = std::pin::pin!(resumed(client, response, path, after));
        while let Some(entry) = entries.next().await {
            yield entry?;
        }
    }
}

/// The log at `path` asked for, after its `after`th entry if given.
async fn open(
    client: &TransportClient,
    path: &str,
    after: Option<u64>,
) -> Result<http::Response<crate::transport::Body>, Error> {
    let mut request = http::Request::get(path).header(header::ACCEPT, "text/event-stream");
    if let Some(after) = after {
        request = request.header("Last-Event-ID", after.to_string());
    }
    let request = request
        .body(full(Vec::new()))
        .map_err(|e| Error::Transport(format!("not a request: {e}")))?;
    let response = client.send(request).await?;
    if !response.status().is_success() {
        return Err(refused(response).await);
    }
    Ok(response)
}

/// The entries of `response`, a log of the job at `path` answered after its `after`th entry
/// if given -- and, where its connection breaks, those the log at `path` answers after the last
/// one read, asked for again up to [`RESUMES`] times in a row, a little longer each time.
pub(crate) fn resumed<'a, E: DeserializeOwned + Send + 'a>(
    client: &'a TransportClient,
    response: http::Response<crate::transport::Body>,
    path: String,
    after: Option<u64>,
) -> impl Stream<Item = Result<LogEntry<E>, Error>> + Send + use<'a, E> {
    async_stream::try_stream! {
        let (mut response, mut after, mut broken) = (Some(response), after, 0);
        loop {
            let answered = match response.take() {
                Some(answered) => Ok(answered),
                None => open(client, &path, after).await,
            };
            let broke = match answered {
                Ok(answered) => {
                    let mut entries = std::pin::pin!(entries(answered, path.clone()));
                    let mut broke = None;
                    while let Some(entry) = entries.next().await {
                        match entry {
                            Ok(entry) => {
                                after = Some(entry.id);
                                broken = 0;
                                yield entry;
                            }
                            Err(Error::Transport(e)) => {
                                broke = Some(Error::Transport(e));
                                break;
                            }
                            Err(e) => Err(e)?,
                        }
                    }
                    match broke {
                        None => return,
                        Some(e) => e,
                    }
                }
                // A server not reached, or one gone for a moment: as a break.
                Err(e @ Error::Transport(_)) => e,
                Err(Error::Status(status @ (502..=504), why)) => Error::Status(status, why),
                Err(e) => Err(e)?,
            };
            if broken == RESUMES {
                Err(broke)?;
            }
            broken += 1;
            tokio::time::sleep(std::time::Duration::from_millis(250 << broken)).await;
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
