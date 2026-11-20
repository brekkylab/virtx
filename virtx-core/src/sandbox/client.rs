//! The client's end of `/sandboxes/` and the jobs in each sandbox.

use ::http::header;
use futures_util::{Stream, StreamExt as _};
use sse_stream::SseStream;

use super::{
    Sandbox, SandboxCreateArgs, SandboxExecArgs, SandboxExecItem, SandboxJob, SandboxJobEntry,
    SandboxResizeArgs, SandboxResources, SandboxSignalArgs, SandboxStdinArgs,
};
use crate::{
    Error,
    transport::{LogEntry, TransportClient, full, http, segment, sse},
};

/// The path of the sandbox `id`.
fn sandbox(id: &str) -> String {
    format!("/sandboxes/{}", segment(id))
}

/// The path of the job `job` in the sandbox `id`.
fn job(id: &str, job: &str) -> String {
    format!("{}/jobs/{}", sandbox(id), segment(job))
}

/// Every sandbox the server holds.
pub(crate) async fn list(client: &TransportClient) -> Result<Vec<Sandbox>, Error> {
    http::get(client, "/sandboxes/").await
}

/// The sandbox named `name`, if one is: of what the server answers, the one named so. Two named
/// so is an answer this cannot read, since a name is one sandbox's.
pub(crate) async fn find(client: &TransportClient, name: &str) -> Result<Option<Sandbox>, Error> {
    let found: Vec<Sandbox> =
        http::get(client, &format!("/sandboxes/?name={}", segment(name))).await?;
    let mut named = found
        .into_iter()
        .filter(|sandbox| sandbox.name.as_deref() == Some(name));
    let first = named.next();
    if let Some(other) = named.next() {
        return Err(Error::Client(format!(
            "the server answered sandboxes {} and {} both named {name}, which is one sandbox's",
            first.map(|sandbox| sandbox.id).unwrap_or_default(),
            other.id
        )));
    }
    Ok(first)
}

/// Make a sandbox as `args` says.
pub(crate) async fn create(
    client: &TransportClient,
    args: &SandboxCreateArgs,
) -> Result<Sandbox, Error> {
    http::post(client, "/sandboxes/", args).await
}

/// The sandbox `id`.
pub(crate) async fn get(client: &TransportClient, id: &str) -> Result<Sandbox, Error> {
    http::get(client, &sandbox(id)).await
}

/// Resize the sandbox `id` as `args` says.
pub(crate) async fn resize(
    client: &TransportClient,
    id: &str,
    args: &SandboxResizeArgs,
) -> Result<SandboxResources, Error> {
    http::patch(client, &sandbox(id), args).await
}

/// Stop the sandbox `id`, end its jobs, and forget it, with everything it wrote outside
/// its volumes.
pub(crate) async fn remove(client: &TransportClient, id: &str) -> Result<(), Error> {
    http::delete(client, &sandbox(id)).await
}

/// Every job in the sandbox `id`.
pub(crate) async fn list_job(client: &TransportClient, id: &str) -> Result<Vec<SandboxJob>, Error> {
    http::get(client, &format!("{}/jobs/", sandbox(id))).await
}

/// Start a process in the sandbox `id`, as `args` says.
pub(crate) async fn exec(
    client: &TransportClient,
    id: &str,
    args: &SandboxExecArgs,
) -> Result<SandboxJob, Error> {
    http::post(client, &format!("{}/jobs/exec/", sandbox(id)), args).await
}

/// Start a process in the sandbox `id`, as `args` says, and read its log: its job, then each
/// entry, until its end. Asked of the server in one request where it answers an exec with its
/// log -- accepting `text/event-stream`, the job its first event -- and in two, the job and then
/// its log, where it answers with the job alone.
pub(crate) fn exec_log<'a>(
    client: &'a TransportClient,
    id: &str,
    args: &SandboxExecArgs,
) -> impl Stream<Item = Result<SandboxExecItem, Error>> + Send + use<'a> {
    let (id, path) = (id.to_owned(), format!("{}/jobs/exec/", sandbox(id)));
    let body = serde_json::to_vec(args).map_err(|e| Error::Client(e.to_string()));
    async_stream::try_stream! {
        let request = ::http::Request::post(&path)
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "text/event-stream, application/json")
            .body(full(body?))
            .map_err(|e| Error::Client(format!("not a request: {e}")))?;
        let response = client.send(request).await?;
        if !response.status().is_success() {
            Err::<(), _>(http::refused(response).await)?;
            return;
        }
        let with_log = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|kind| kind.to_str().ok())
            .is_some_and(|kind| kind.starts_with("text/event-stream"));
        if !with_log {
            let job: SandboxJob = http::json(response).await?;
            let job_id = job.id.clone();
            yield SandboxExecItem::Job(job);
            let mut log = std::pin::pin!(job_log(client, &id, &job_id, None));
            while let Some(entry) = log.next().await {
                yield SandboxExecItem::Log(entry?);
            }
            return;
        }
        let location = response
            .headers()
            .get(header::LOCATION)
            .and_then(|at| at.to_str().ok())
            .map(str::to_owned);
        let mut events = SseStream::new(response.into_body());
        // The job, the answer's first event; where the answer breaks or ends before it, the job
        // at its path in `Location`.
        let first = loop {
            match events.next().await {
                Some(Ok(event)) => match event.data {
                    Some(data) => break Some(data),
                    None => continue,
                },
                Some(Err(_)) | None => break None,
            }
        };
        let job: SandboxJob = match first {
            Some(first) => match serde_json::from_str(&first).map_err(|e| Error::Client(e.to_string()))? {
                SandboxExecItem::Job(job) => job,
                SandboxExecItem::Log(_) => Err(Error::Client(format!("{path} answered its log without its job first")))?,
            },
            None => {
                let location = location.ok_or_else(|| {
                    Error::Transport(format!("{path} broke off before its job, and named no `Location`"))
                })?;
                http::get(client, &location).await?
            }
        };
        let at = format!("{}/events", self::job(&id, &job.id));
        let job_id = job.id.clone();
        yield SandboxExecItem::Job(job);
        // Its log, from this answer, until its `end`, or until the body breaks or ends before it.
        let (mut after, mut ended) = (None, false);
        let mut entries = std::pin::pin!(sse::client::entries(events, &at));
        loop {
            match entries.next().await {
                None if ended => return,
                None => break,
                Some(Ok((entry, end))) => {
                    (after, ended) = (Some(entry.id), end);
                    yield SandboxExecItem::Log(entry);
                    if end {
                        return;
                    }
                }
                Some(Err(Error::Transport(_))) => break,
                Some(Err(e)) => Err(e)?,
            }
        }
        // The answer broke, or ended before the job's end: the job's log, after the last entry
        // read -- the job's event is none of the log's, and moves no count -- as a log is read on.
        let mut log = std::pin::pin!(job_log(client, &id, &job_id, after));
        while let Some(entry) = log.next().await {
            yield SandboxExecItem::Log(entry?);
        }
    }
}

/// The job `job` in the sandbox `id`.
pub(crate) async fn get_job(
    client: &TransportClient,
    id: &str,
    job: &str,
) -> Result<SandboxJob, Error> {
    http::get(client, &self::job(id, job)).await
}

/// Cancel the job `job` in the sandbox `id` if it runs, killing its process, and forget it.
pub(crate) async fn remove_job(client: &TransportClient, id: &str, job: &str) -> Result<(), Error> {
    http::delete(client, &self::job(id, job)).await
}

/// Send `args`'s signal to the process of the job `job` in the sandbox `id`.
pub(crate) async fn signal_job(
    client: &TransportClient,
    id: &str,
    job: &str,
    args: &SandboxSignalArgs,
) -> Result<(), Error> {
    http::post_empty(client, &format!("{}/signal", self::job(id, job)), args).await
}

/// Write `args` to the stdin of the process of the job `job` in the sandbox `id`. Answers once
/// it is written; the next write waits for it.
pub(crate) async fn write_job_stdin(
    client: &TransportClient,
    id: &str,
    job: &str,
    args: &SandboxStdinArgs,
) -> Result<(), Error> {
    http::post_empty(client, &format!("{}/stdin", self::job(id, job)), args).await
}

/// The log of the job `job` in the sandbox `id`, after its `after`th entry if given, until its
/// end.
pub(crate) fn job_log<'a>(
    client: &'a TransportClient,
    id: &str,
    job: &str,
    after: Option<u64>,
) -> impl Stream<Item = Result<LogEntry<SandboxJobEntry>, Error>> + Send + use<'a> {
    sse::client::log(client, format!("{}/events", self::job(id, job)), after)
}
