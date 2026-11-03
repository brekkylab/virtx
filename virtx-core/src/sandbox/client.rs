//! The client's end of `/sandboxes/` and the jobs in each sandbox.

use futures_util::Stream;

use super::{
    Sandbox, SandboxCreateArgs, SandboxExecArgs, SandboxJob, SandboxJobEntry, SandboxSignalArgs,
    SandboxStdinArgs,
};
use crate::{
    Error,
    transport::{LogEntry, TransportClient, http, segment, sse},
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

/// Stop the sandbox `id`, end its jobs, and forget it, keeping its snapshot and volumes.
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
