//! The client's end of a job's log: its event stream, read as entries.

use futures_util::{Stream, StreamExt};
use http::header;
use serde::de::DeserializeOwned;
use sse_stream::SseStream;

use crate::LogEntry;
use crate::{
    Error,
    transport::{Body, TransportClient, full, http::refused},
};

/// How many times in a row a log whose connection broke is asked for again, after what was read
/// of it, before the break is the caller's: a server's process replaced, or the connection to
/// it lost, leaves the job running, and its log is all still there.
const RESUMES: u32 = 5;

/// The entries of the log at `path`, each read as an `E`, after the `after`th if given: as
/// many as the server sends, until its `end`. A connection that breaks on the way, over TCP, is
/// opened again after the last entry read, up to [`RESUMES`] times in a row, a little longer
/// apart each time: the server gone for a moment, or a proxy in front of it answering `502` to
/// `504` for it, is as a break. Over stdio, the connection is the server's process, and a break
/// is the caller's at once.
///
/// A stream the server ends before the `end`, having sent entries, is opened again at once after
/// the last: one cut on the way reads on, and one whose job was removed is answered `404`. One
/// that ends with nothing past where it was asked is read on only while its job runs, as a
/// break: a job removed is its `404`, and one that has ended has nothing more, and its log ends
/// there.
pub(crate) fn log<'a, E: DeserializeOwned + Send + 'a>(
    client: &'a TransportClient,
    path: String,
    after: Option<u64>,
) -> impl Stream<Item = Result<LogEntry<E>, Error>> + Send + use<'a, E> {
    async_stream::try_stream! {
        let mut response = Some(open(client, &path, after).await?);
        let (mut after, mut broken) = (after, 0);
        loop {
            let opened = match response.take() {
                Some(opened) => Ok(opened),
                None => open(client, &path, after).await,
            };
            // Why it is opened again, and whether that is its connection broken.
            let (broke, cut) = match opened {
                Ok(opened) => {
                    let opened = SseStream::new(opened.into_body());
                    let mut entries = std::pin::pin!(entries::<E>(opened, &path));
                    let (mut broke, mut read, mut ended) = (None, false, false);
                    while let Some(entry) = entries.next().await {
                        match entry {
                            Ok((entry, end)) => {
                                after = Some(entry.id);
                                (broken, read, ended) = (0, true, end);
                                yield entry;
                            }
                            Err(e @ Error::Transport(_)) => {
                                broke = Some(e);
                                break;
                            }
                            Err(e) => Err(e)?,
                        }
                    }
                    match broke {
                        Some(e) => (e, true),
                        None if ended => return,
                        None if read => continue,
                        None if !running(client, &path).await? => return,
                        None => (Error::Transport(format!("{path} ended before its job")), false),
                    }
                }
                Err(e @ Error::Transport(_)) => (e, true),
                Err(e) if matches!(e.status(), Some(502..=504)) => (e, true),
                Err(e) => Err(e)?,
            };
            if broken == RESUMES || cut && !client.reconnects() {
                Err(broke)?;
            }
            broken += 1;
            tokio::time::sleep(std::time::Duration::from_millis(250 << broken)).await;
        }
    }
}

/// The log at `path` asked for, after its `after`th entry if given.
async fn open(
    client: &TransportClient,
    path: &str,
    after: Option<u64>,
) -> Result<http::Response<Body>, Error> {
    let mut request = http::Request::get(path).header(header::ACCEPT, "text/event-stream");
    if let Some(after) = after {
        request = request.header("Last-Event-ID", after.to_string());
    }
    let request = request
        .body(full(Vec::new()))
        .map_err(|e| Error::Client(format!("not a request: {e}")))?;
    let response = client.send(request).await?;
    if !response.status().is_success() {
        return Err(refused(response).await);
    }
    Ok(response)
}

/// Whether the job whose log is at `path`, `{job}/events`, runs still. A job removed is its
/// refusal, `404`.
async fn running(client: &TransportClient, path: &str) -> Result<bool, Error> {
    let job = path.strip_suffix("/events").unwrap_or(path);
    let job: serde_json::Value = crate::transport::http::get(client, job).await?;
    Ok(job["running"].as_bool().unwrap_or(false))
}

/// The entries of `events`, a log answered from `path`, each read as an `E`, and whether it is
/// the log's `end`, until its body ends. A connection broken on the way is an
/// [`Error::Transport`].
pub(crate) fn entries<'a, E: DeserializeOwned + Send + 'a>(
    mut events: SseStream<Body>,
    path: &'a str,
) -> impl Stream<Item = Result<(LogEntry<E>, bool), Error>> + Send + 'a {
    async_stream::try_stream! {
        while let Some(event) = events.next().await {
            let event = event.map_err(|e| Error::Transport(format!("reading the log: {e}")))?;
            let Some(data) = event.data else {
                continue;
            };
            let id = event
                .id
                .and_then(|id| id.parse().ok())
                .ok_or_else(|| Error::Client(format!("an entry of {path} without its id")))?;
            let read: serde_json::Value =
                serde_json::from_str(&data).map_err(|e| Error::Client(e.to_string()))?;
            let end = read.get("end").is_some();
            let entry = serde_json::from_value(read).map_err(|e| Error::Client(e.to_string()))?;
            yield (LogEntry { id, entry }, end);
        }
    }
}
