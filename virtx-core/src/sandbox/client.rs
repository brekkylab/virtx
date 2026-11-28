//! The client's end of `/sandboxes/` and the jobs in each sandbox.

use ::http::header;
use futures_util::{Stream, StreamExt as _};
use sse_stream::SseStream;

use super::{
    Sandbox, SandboxCreateArgs, SandboxExecArgs, SandboxExecItem, SandboxJob, SandboxJobEntry,
    SandboxResizeArgs, SandboxResources, SandboxSignalArgs, SandboxStdinArgs,
};
use crate::{Client, Error, LogEntry};
use http_over_x::{full, segment};

/// The path of the sandbox `id`.
fn sandbox(id: &str) -> String {
    format!("/sandboxes/{}", segment(id))
}

/// The path of the job `job` in the sandbox `id`.
fn job(id: &str, job: &str) -> String {
    format!("{}/jobs/{}", sandbox(id), segment(job))
}

/// Every sandbox the server holds.
pub(crate) async fn list(client: &Client) -> Result<Vec<Sandbox>, Error> {
    Ok(client.wire().get("/sandboxes/", client.headers()).await?)
}

/// The sandbox named `name`, if one is: of what the server answers, the one named so. Two named
/// so is an answer this cannot read, since a name is one sandbox's.
pub(crate) async fn find(client: &Client, name: &str) -> Result<Option<Sandbox>, Error> {
    let found: Vec<Sandbox> = client
        .wire()
        .get(
            &format!("/sandboxes/?name={}", segment(name)),
            client.headers(),
        )
        .await?;
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
pub(crate) async fn create(client: &Client, args: &SandboxCreateArgs) -> Result<Sandbox, Error> {
    Ok(client
        .wire()
        .post("/sandboxes/", client.headers(), args)
        .await?)
}

/// The sandbox `id`.
pub(crate) async fn get(client: &Client, id: &str) -> Result<Sandbox, Error> {
    Ok(client.wire().get(&sandbox(id), client.headers()).await?)
}

/// Resize the sandbox `id` as `args` says.
pub(crate) async fn resize(
    client: &Client,
    id: &str,
    args: &SandboxResizeArgs,
) -> Result<SandboxResources, Error> {
    Ok(client
        .wire()
        .patch(&sandbox(id), client.headers(), args)
        .await?)
}

/// Stop the sandbox `id`, end its jobs, and forget it, with everything it wrote outside
/// its volumes.
pub(crate) async fn remove(client: &Client, id: &str) -> Result<(), Error> {
    Ok(client.wire().delete(&sandbox(id), client.headers()).await?)
}

/// Every job in the sandbox `id`.
pub(crate) async fn list_job(client: &Client, id: &str) -> Result<Vec<SandboxJob>, Error> {
    Ok(client
        .wire()
        .get(&format!("{}/jobs/", sandbox(id)), client.headers())
        .await?)
}

/// Start a process in the sandbox `id`, as `args` says.
pub(crate) async fn exec(
    client: &Client,
    id: &str,
    args: &SandboxExecArgs,
) -> Result<SandboxJob, Error> {
    Ok(client
        .wire()
        .post(
            &format!("{}/jobs/exec/", sandbox(id)),
            client.headers(),
            args,
        )
        .await?)
}

/// Start a process in the sandbox `id`, as `args` says, and read its log: its job, then each
/// entry, until its end. Asked of the server in one request where it answers an exec with its
/// log -- accepting `text/event-stream`, the job its first event -- and in two, the job and then
/// its log, where it answers with the job alone.
pub(crate) fn exec_log<'a>(
    client: &'a Client,
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
            Err::<(), _>(http_over_x::refused(response).await)?;
            return;
        }
        let with_log = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|kind| kind.to_str().ok())
            .is_some_and(|kind| kind.starts_with("text/event-stream"));
        if !with_log {
            let job: SandboxJob = http_over_x::json(response).await?;
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
                client.wire().get(&location, client.headers()).await?
            }
        };
        let job_id = job.id.clone();
        yield SandboxExecItem::Job(job);
        // Its log, from this answer, until its `end`, or until the body breaks or ends before it.
        let (mut after, mut ended) = (None, false);
        let mut events = std::pin::pin!(http_over_x::sse::read(events));
        loop {
            match events.next().await {
                None if ended => return,
                None => break,
                Some(Ok(event)) => {
                    let (entry, end) = LogEntry::read(event)?;
                    (after, ended) = (Some(entry.id), end);
                    yield SandboxExecItem::Log(entry);
                }
                Some(Err(http_over_x::Error::Transport(_))) => break,
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
pub(crate) async fn get_job(client: &Client, id: &str, job: &str) -> Result<SandboxJob, Error> {
    Ok(client
        .wire()
        .get(&self::job(id, job), client.headers())
        .await?)
}

/// Cancel the job `job` in the sandbox `id` if it runs, killing its process, and forget it.
pub(crate) async fn remove_job(client: &Client, id: &str, job: &str) -> Result<(), Error> {
    Ok(client
        .wire()
        .delete(&self::job(id, job), client.headers())
        .await?)
}

/// Send `args`'s signal to the process of the job `job` in the sandbox `id`.
pub(crate) async fn signal_job(
    client: &Client,
    id: &str,
    job: &str,
    args: &SandboxSignalArgs,
) -> Result<(), Error> {
    Ok(client
        .wire()
        .post_empty(
            &format!("{}/signal", self::job(id, job)),
            client.headers(),
            args,
        )
        .await?)
}

/// Write `args` to the stdin of the process of the job `job` in the sandbox `id`. Answers once
/// it is written; the next write waits for it.
pub(crate) async fn write_job_stdin(
    client: &Client,
    id: &str,
    job: &str,
    args: &SandboxStdinArgs,
) -> Result<(), Error> {
    Ok(client
        .wire()
        .post_empty(
            &format!("{}/stdin", self::job(id, job)),
            client.headers(),
            args,
        )
        .await?)
}

/// The log of the job `job` in the sandbox `id`, after its `after`th entry if given, until its
/// end.
pub(crate) fn job_log<'a>(
    client: &'a Client,
    id: &str,
    job: &str,
    after: Option<u64>,
) -> impl Stream<Item = Result<LogEntry<SandboxJobEntry>, Error>> + Send + use<'a> {
    crate::log_entry::log(client, self::job(id, job), after)
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use ::http::HeaderMap;
    use axum::{Json, Router, routing::get};
    use tokio::net::TcpListener;

    use super::*;
    use crate::Auth;

    /// `router` served over HTTP on this machine's loopback, and its base URL.
    async fn serve(router: Router) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, router).await });
        url
    }

    /// A sandbox is found by its name in `?name=`, encoded; of what a server answers, only one
    /// named so is taken, and two named so are refused.
    #[tokio::test]
    async fn a_sandbox_is_found_by_its_name_encoded() {
        use axum::extract::RawQuery;
        use serde_json::json;

        let asked = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let list = {
            let asked = asked.clone();
            move |RawQuery(query): RawQuery| async move {
                asked.lock().unwrap().push(query);
                // Every sandbox, as a server that reads no `name` answers.
                Json(json!([
                    {"id": "s1", "image": "sha256:00", "running": true},
                    {"id": "s2", "name": "a b&c", "image": "sha256:00", "running": true},
                    {"id": "s3", "name": "twice", "image": "sha256:00", "running": true},
                    {"id": "s4", "name": "twice", "image": "sha256:00", "running": true},
                ]))
            }
        };
        let url = serve(Router::new().route("/sandboxes/", get(list))).await;
        let client = Client::connect(&url, Auth::None).unwrap();

        let found = find(&client, "a b&c").await.unwrap();
        assert_eq!(found.map(|s| s.id).as_deref(), Some("s2"));
        let found = find(&client, "s1").await.unwrap();
        assert_eq!(found, None);
        let twice = find(&client, "twice").await;
        assert!(matches!(twice, Err(crate::Error::Client(_))), "{twice:?}");
        assert_eq!(
            *asked.lock().unwrap(),
            [
                Some("name=a%20b%26c".to_owned()),
                Some("name=s1".to_owned()),
                Some("name=twice".to_owned())
            ]
        );
    }

    /// An exec is read with its log in one request from a server that answers it so, the job its
    /// first event; from one that answers the job alone, in two; and where the answer breaks, the
    /// job's log is read on after the last entry read, the job's event counting for none -- and
    /// the job itself read at its `Location` where the answer breaks before it.
    #[tokio::test]
    async fn an_exec_is_read_with_its_log_in_one_request_or_two() {
        use std::sync::{Arc, Mutex};

        use axum::{body::Body, routing::post};
        use futures_util::{StreamExt as _, TryStreamExt as _};

        use crate::{SandboxExecArgs, SandboxExecItem, SandboxJobEntry};

        const JOB: &str = r#"{"id":"j","kind":"exec","pid":1,"cmd":["x"],"running":true}"#;
        let entries = |from: u64, to: u64| -> String {
            (from..=to)
                .map(|id| match id {
                    3 => format!("id: {id}\ndata: {{\"end\":{{\"code\":0}}}}\n\n"),
                    _ => format!("id: {id}\ndata: {{\"stdout\":\"{id}\"}}\n\n"),
                })
                .collect()
        };
        let job_event = format!("data: {{\"job\":{JOB}}}\n\n");
        // `mode`: `log`, answered with all of it; `job`, with the job alone; `broken`, with the
        // job and the first entry, then a broken body; `jobless`, with a body broken before the
        // job; `bare`, with the job, then a broken body.
        let asked = Arc::new(Mutex::new(Vec::<String>::new()));
        let router = |mode: &'static str| {
            let asked = asked.clone();
            let job_event = job_event.clone();
            let exec = {
                let asked = asked.clone();
                move |headers: HeaderMap| async move {
                    let accept = headers["accept"].to_str().unwrap().to_owned();
                    asked.lock().unwrap().push(format!("exec {accept}"));
                    // Broken a while after `sent`, once that has reached the client.
                    let broken = |sent: String| {
                        let sent = futures_util::stream::iter([Ok(sent)]);
                        let gone = futures_util::stream::once(async {
                            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                            Err(std::io::Error::other("gone"))
                        });
                        Body::from_stream(sent.chain(gone))
                    };
                    let log = |body: Body| {
                        axum::response::Response::builder()
                            .status(201)
                            .header("content-type", "text/event-stream")
                            .header("location", "/sandboxes/s/jobs/j")
                            .body(body)
                            .unwrap()
                    };
                    match mode {
                        "log" => log(Body::from(format!("{job_event}{}", entries(1, 3)))),
                        "broken" => log(broken(format!("{job_event}{}", entries(1, 1)))),
                        "jobless" => log(broken(": open\n\n".into())),
                        "bare" => log(broken(job_event)),
                        _ => axum::response::Response::builder()
                            .status(201)
                            .header("content-type", "application/json")
                            .header("location", "/sandboxes/s/jobs/j")
                            .body(Body::from(JOB))
                            .unwrap(),
                    }
                }
            };
            let got = {
                let asked = asked.clone();
                move || async move {
                    asked.lock().unwrap().push("job".into());
                    ([("content-type", "application/json")], JOB)
                }
            };
            let events = move |headers: HeaderMap| async move {
                let after = headers
                    .get("last-event-id")
                    .map(|after| after.to_str().unwrap().parse::<u64>().unwrap());
                asked.lock().unwrap().push(match after {
                    Some(after) => format!("events after {after}"),
                    None => "events".into(),
                });
                (
                    [("content-type", "text/event-stream")],
                    entries(after.unwrap_or(0) + 1, 3),
                )
            };
            Router::new()
                .route("/sandboxes/s/jobs/exec/", post(exec))
                .route("/sandboxes/s/jobs/j", get(got))
                .route("/sandboxes/s/jobs/j/events", get(events))
        };
        let args = SandboxExecArgs::new(["x"]);
        let exec = "exec text/event-stream, application/json";
        for (mode, requests) in [
            ("log", vec![exec]),
            ("job", vec![exec, "events"]),
            ("broken", vec![exec, "events after 1"]),
            ("jobless", vec![exec, "job", "events"]),
            ("bare", vec![exec, "events"]),
        ] {
            asked.lock().unwrap().clear();
            let url = serve(router(mode)).await;
            let client = Client::connect(&url, Auth::None).unwrap();
            let read: Vec<_> = exec_log(&client, "s", &args).try_collect().await.unwrap();
            assert!(
                matches!(&read[0], SandboxExecItem::Job(job) if job.id == "j"),
                "{mode}: {read:?}"
            );
            let ids: Vec<_> = read[1..]
                .iter()
                .map(|item| match item {
                    SandboxExecItem::Log(entry) => entry.id,
                    SandboxExecItem::Job(_) => panic!("{mode}: a job after the first"),
                })
                .collect();
            assert_eq!(ids, [1, 2, 3], "{mode}");
            assert!(matches!(
                read.last(),
                Some(SandboxExecItem::Log(entry)) if matches!(entry.entry, SandboxJobEntry::End(_))
            ));
            assert_eq!(*asked.lock().unwrap(), requests, "{mode}");
        }
    }
}
