//! The client's end of `/sandboxes/` and the jobs in each sandbox.

use futures_util::Stream;

use ::http::header;
use futures_util::StreamExt as _;

use super::{
    Sandbox, SandboxCreateArgs, SandboxExecArgs, SandboxExecItem, SandboxJob, SandboxJobEntry,
    SandboxPort, SandboxPortOpenArgs, SandboxSignalArgs, SandboxStdinArgs,
};
use crate::{
    Error,
    transport::{LogEntry, TransportClient, full, http, segment, sse},
};

/// Where a server that answers an exec with its log says its job: virtx cloud's, beside the
/// protocol.
const JOB_HEADER: &str = "virtx-job";

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

/// The sandbox named `name`, if one is.
pub(crate) async fn find(client: &TransportClient, name: &str) -> Result<Option<Sandbox>, Error> {
    let found: Vec<Sandbox> =
        http::get(client, &format!("/sandboxes/?name={}", segment(name))).await?;
    Ok(found.into_iter().next())
}

/// Start the sandbox `id`, if it is stopped: answered once it runs, with where its ports are.
pub(crate) async fn start(client: &TransportClient, id: &str) -> Result<Sandbox, Error> {
    http::post(client, &format!("{}/start", sandbox(id)), &Empty {}).await
}

/// Stop the sandbox `id`, if it runs: answered once it has stopped, its disk kept -- or, for
/// an ephemeral one, deleted.
pub(crate) async fn stop(client: &TransportClient, id: &str) -> Result<Sandbox, Error> {
    http::post(client, &format!("{}/stop", sandbox(id)), &Empty {}).await
}

/// `{}`: the body `start` and `stop` take.
#[derive(serde::Serialize)]
struct Empty {}

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

/// Delete the sandbox `id`: stopped first if it runs, and its disk removed. Its volumes stay.
pub(crate) async fn remove(client: &TransportClient, id: &str) -> Result<(), Error> {
    http::delete(client, &sandbox(id)).await
}

/// Open the TCP port `port` in the sandbox `id`, as those it was made with are: where it is
/// reached from.
pub(crate) async fn open_port(
    client: &TransportClient,
    id: &str,
    port: u16,
    args: &SandboxPortOpenArgs,
) -> Result<SandboxPort, Error> {
    http::put(client, &format!("{}/ports/{port}", sandbox(id)), args).await
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
/// log -- accepting `text/event-stream`, the job in [`JOB_HEADER`] -- and in two, the job and
/// then its log, where it answers with the job alone, as the protocol says.
pub(crate) fn exec_log<'a>(
    client: &'a TransportClient,
    id: &str,
    args: &SandboxExecArgs,
) -> impl Stream<Item = Result<SandboxExecItem, Error>> + Send + use<'a> {
    let (id, path) = (id.to_owned(), format!("{}/jobs/exec/", sandbox(id)));
    let body = serde_json::to_vec(args).map_err(|e| Error::Json(e.to_string()));
    async_stream::try_stream! {
        let request = ::http::Request::post(&path)
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "text/event-stream, application/json")
            .body(full(body?))
            .map_err(|e| Error::Transport(format!("not a request: {e}")))?;
        let response = client.send(request).await?;
        if !response.status().is_success() {
            Err::<(), _>(http::refused(response).await)?;
            return;
        }
        let with_log = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|c| c.to_str().ok())
            .is_some_and(|c| c.starts_with("text/event-stream"));
        if !with_log {
            let job: SandboxJob = http::json(response).await?;
            yield SandboxExecItem::Job(job.clone());
            let mut log = std::pin::pin!(job_log(client, &id, &job.id, None));
            while let Some(entry) = log.next().await {
                yield SandboxExecItem::Log(entry?);
            }
            return;
        }
        let job: SandboxJob = response
            .headers()
            .get(JOB_HEADER)
            .and_then(|j| j.to_str().ok())
            .ok_or_else(|| Error::Transport(format!("{path} answered its log without its job")))
            .and_then(|j| serde_json::from_str(j).map_err(|e| Error::Json(e.to_string())))?;
        yield SandboxExecItem::Job(job);
        let mut log = std::pin::pin!(sse::client::entries(response, path));
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
