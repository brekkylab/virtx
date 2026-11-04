//! The test server: every endpoint of `PROTOCOL.md` but the images' registry, in memory, as an
//! axum router for the crate's own tests and for the `test_server` example the bindings' tests
//! spawn.
//!
//! It starts with the volumes `v` and `s`, each a filesystem of its own, and no sandbox. A
//! sandbox is made running, stopped by its `stop` -- deleted then, if ephemeral -- and started
//! by its `start` or a job started in it; nothing stops one for want of use.
//!
//! Its jobs pretend, and keep a log of at most [`LOG_LIMIT`] entries before their end:
//!
//! - A build logs `pulling <base>`, and fails for the base `missing`; a pull logs
//!   `pulling <reference>` and fails for `missing`; a commit logs `committing <sandbox>`, and
//!   fails for a sandbox that is not; a push logs `pushing <reference> as <auth>`,
//!   the type of its credentials or `anonymous`. Each succeeds with `sha256:0`, but for the base
//!   or reference `hang`, which runs until it is cancelled. A prune deletes `sha256:0`, of 1024
//!   bytes.
//! - In a sandbox, `echo` writes its arguments as a line to stdout, `fail` writes `failed` to
//!   stderr and ends with 1, `lines n` writes `line 1` up to `line n`, `cat` writes each line of
//!   its stdin back until it is closed, and `sleep` runs until it is signalled, cancelled or timed
//!   out. Any other command is not started.

use std::{
    collections::BTreeMap,
    convert::Infallible,
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{
    Router,
    extract::{Path, State},
    response::{IntoResponse, Response},
    routing::{any, get, post},
};
use bytes::Bytes;
use dav_server::{DavHandler, memfs::MemFs};
use futures_util::Stream;
use http::{HeaderMap, HeaderValue, StatusCode, header};
use serde::{Serialize, de::DeserializeOwned};
use sse_stream::{Sse, SseBody};
use tokio::sync::watch;

use crate::{
    image::{
        ImageBuildArgs, ImageCommitArgs, ImageJob, ImageJobEnd, ImageJobEntry, ImageJobKind,
        ImageName, ImagePruneOutput, ImagePullArgs, ImagePushArgs, RegistryAuth,
    },
    sandbox::{
        Sandbox, SandboxCreateArgs, SandboxExecArgs, SandboxJob, SandboxJobEnd, SandboxJobEntry,
        SandboxJobKind, SandboxPort, SandboxSignalArgs, SandboxState, SandboxStdinArgs,
    },
    volume::{
        VolumeCreateArgs, VolumeListOutputItem, VolumeMountCreateArgs, VolumeMountListOutputItem,
    },
};

/// How many entries a job's log keeps before its end.
///
/// cbindgen:ignore
pub const LOG_LIMIT: usize = 8;

/// A job's log, and who waits on it.
struct Log<E> {
    state: Mutex<LogState<E>>,
    changed: watch::Sender<()>,
}

struct LogState<E> {
    entries: Vec<E>,
    truncated: bool,
    ended: bool,
    /// The job was removed: a reader's stream ends where it is.
    removed: bool,
}

impl<E: Clone> Log<E> {
    fn new() -> Arc<Log<E>> {
        Arc::new(Log {
            state: Mutex::new(LogState {
                entries: Vec::new(),
                truncated: false,
                ended: false,
                removed: false,
            }),
            changed: watch::channel(()).0,
        })
    }

    /// Keep `entry`, unless the log is past its limit, where `truncated` is kept once instead.
    fn push(&self, entry: E, truncated: E) {
        let mut state = self.state.lock().unwrap();
        if state.ended {
            return;
        }
        if state.entries.len() < LOG_LIMIT {
            state.entries.push(entry);
        } else if !state.truncated {
            state.truncated = true;
            state.entries.push(truncated);
        }
        drop(state);
        self.changed.send_replace(());
    }

    /// Keep `end` last. False if the log had ended already.
    fn end(&self, end: E) -> bool {
        let mut state = self.state.lock().unwrap();
        if state.ended {
            return false;
        }
        state.ended = true;
        state.entries.push(end);
        drop(state);
        self.changed.send_replace(());
        true
    }

    fn remove(&self) {
        self.state.lock().unwrap().removed = true;
        self.changed.send_replace(());
    }

    /// The entries after the `after`th, as each comes, until the end or the job's removal.
    fn read(self: Arc<Self>, after: usize) -> impl Stream<Item = (usize, E)> + Send
    where
        E: Send + 'static,
    {
        async_stream::stream! {
            let mut changed = self.changed.subscribe();
            let mut next = after;
            loop {
                let (entries, done) = {
                    let state = self.state.lock().unwrap();
                    let entries = state.entries.get(next..).unwrap_or_default().to_vec();
                    // Ended, every entry is among these.
                    (entries, state.removed || state.ended)
                };
                for entry in entries {
                    next += 1;
                    yield (next, entry);
                }
                if done {
                    break;
                }
                if changed.changed().await.is_err() {
                    break;
                }
            }
        }
    }
}

/// An image job, and its log.
struct ImageJobState {
    job: Mutex<ImageJob>,
    log: Arc<Log<ImageJobEntry>>,
}

impl ImageJobState {
    fn end(&self, end: ImageJobEnd) {
        {
            let mut job = self.job.lock().unwrap();
            job.running = false;
            job.digest.clone_from(&end.digest);
            job.error.clone_from(&end.error);
        }
        self.log.end(ImageJobEntry::End(end));
    }
}

/// A sandbox job, and its log.
struct SandboxJobState {
    job: Mutex<SandboxJob>,
    /// Whether its stdin takes writes still.
    stdin: Mutex<bool>,
    log: Arc<Log<SandboxJobEntry>>,
}

impl SandboxJobState {
    fn write(&self, entry: SandboxJobEntry) {
        self.log.push(entry, SandboxJobEntry::Truncated {});
    }

    /// End its process with `code`, if it runs still.
    fn end(&self, code: i32) {
        {
            let mut job = self.job.lock().unwrap();
            if !job.running {
                return;
            }
            job.running = false;
            job.code = Some(code);
        }
        *self.stdin.lock().unwrap() = false;
        self.log.end(SandboxJobEntry::End(SandboxJobEnd { code }));
    }
}

/// A sandbox held here, with where its ports are, which only making and starting it answer.
struct Held {
    sandbox: Sandbox,
    ports: Vec<SandboxPort>,
    jobs: BTreeMap<String, Arc<SandboxJobState>>,
}

impl Held {
    /// Stopped: each job that runs ends as a cancellation does, and is kept.
    fn stop(&mut self) {
        for job in self.jobs.values() {
            job.end(137);
        }
        self.sandbox.state = SandboxState::Stopped;
    }
}

#[derive(Default)]
struct Data {
    next: u64,
    volumes: BTreeMap<String, (DavHandler, BTreeMap<String, VolumeMountListOutputItem>)>,
    sandboxes: BTreeMap<String, Held>,
    image_jobs: BTreeMap<String, Arc<ImageJobState>>,
}

impl Data {
    /// A new id, as `{prefix}{n}`.
    fn id(&mut self, prefix: &str) -> String {
        self.next += 1;
        format!("{prefix}{}", self.next)
    }
}

type Shared = Arc<Mutex<Data>>;

/// A filesystem in memory, served under `prefix`, the root's path such as `/volumes/v/files`.
fn dav(prefix: &str) -> DavHandler {
    DavHandler::builder()
        .filesystem(MemFs::new())
        .strip_prefix(prefix)
        .build_handler()
}

/// Every endpoint but the images' registry, with the volumes `v` and `s` in it.
pub fn router() -> Router {
    let mut data = Data::default();
    for name in ["v", "s"] {
        data.volumes.insert(
            name.into(),
            (dav(&format!("/volumes/{name}/files")), BTreeMap::new()),
        );
    }
    let shared: Shared = Arc::new(Mutex::new(data));
    Router::new()
        .route("/images/jobs/", get(list_image_jobs))
        .route("/images/jobs/{id}/", post(start_image_job))
        .route(
            "/images/jobs/{id}",
            get(get_image_job).delete(remove_image_job),
        )
        .route("/images/jobs/{id}/events", get(image_job_events))
        .route("/images/prune", post(prune))
        .route("/volumes/", get(list_volumes).post(create_volume))
        .route("/volumes/{name}", get(get_volume).delete(remove_volume))
        .route("/volumes/{name}/mounts/", get(list_mounts))
        .route(
            "/volumes/{name}/mounts/{*path}",
            axum::routing::put(create_mount).delete(remove_mount),
        )
        .route("/volumes/{name}/files", any(volume_files))
        .route("/volumes/{name}/files/", any(volume_files))
        .route("/volumes/{name}/files/{*path}", any(volume_files))
        .route("/sandboxes/", get(list_sandboxes).post(create_sandbox))
        .route("/sandboxes/{id}", get(get_sandbox).delete(remove_sandbox))
        .route("/sandboxes/{id}/start", post(start_sandbox))
        .route("/sandboxes/{id}/stop", post(stop_sandbox))
        .route(
            "/sandboxes/{id}/ports/{port}",
            axum::routing::put(open_port),
        )
        .route("/sandboxes/{id}/jobs/", get(list_sandbox_jobs))
        .route("/sandboxes/{id}/jobs/exec/", post(exec))
        .route(
            "/sandboxes/{id}/jobs/{job}",
            get(get_sandbox_job).delete(remove_sandbox_job),
        )
        .route("/sandboxes/{id}/jobs/{job}/signal", post(signal))
        .route("/sandboxes/{id}/jobs/{job}/stdin", post(stdin))
        .route("/sandboxes/{id}/jobs/{job}/events", get(sandbox_job_events))
        .with_state(shared)
}

/// A refusal, as `PROTOCOL.md`'s Resources answers one.
fn refuse(status: StatusCode, code: &str, message: impl Into<String>) -> Response {
    let body = serde_json::json!({"error": {"code": code, "message": message.into()}});
    (status, json(&body)).into_response()
}

fn not_found(what: &str) -> Response {
    refuse(StatusCode::NOT_FOUND, "not_found", format!("no {what}"))
}

/// `value` as a JSON body.
fn json(value: &impl Serialize) -> Response {
    let mut response = serde_json::to_vec(value).unwrap().into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
}

/// `value`, made at `location`.
fn created(location: String, value: &impl Serialize) -> Response {
    let mut response = json(value);
    *response.status_mut() = StatusCode::CREATED;
    response
        .headers_mut()
        .insert(header::LOCATION, location.parse().unwrap());
    response
}

/// `body` read as a `T`, or the refusal it is.
#[allow(
    clippy::result_large_err,
    reason = "the refusal is the response, sent as it is"
)]
fn body<T: DeserializeOwned>(body: &Bytes) -> Result<T, Response> {
    serde_json::from_slice(body)
        .map_err(|e| refuse(StatusCode::BAD_REQUEST, "invalid", e.to_string()))
}

/// The entries of `log` after the one `Last-Event-ID` in `headers` names, as an event stream.
fn events<E: Serialize + Clone + Send + 'static>(
    log: Arc<Log<E>>,
    headers: &HeaderMap,
) -> Response {
    let after = headers
        .get("last-event-id")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let events = futures_util::StreamExt::map(log.read(after), |(id, entry)| {
        let data = serde_json::to_string(&entry).unwrap();
        Ok::<_, Infallible>(Sse::default().id(id.to_string()).data(data))
    });
    let mut response = axum::body::Body::new(SseBody::new(events)).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    response
}

async fn list_image_jobs(State(shared): State<Shared>) -> Response {
    let data = shared.lock().unwrap();
    let jobs: Vec<_> = data
        .image_jobs
        .values()
        .map(|state| state.job.lock().unwrap().clone())
        .collect();
    json(&jobs)
}

async fn start_image_job(
    State(shared): State<Shared>,
    Path(kind): Path<String>,
    request: Bytes,
) -> Response {
    let (kind, reference, line, fails) = match kind.as_str() {
        "build" => match body::<ImageBuildArgs>(&request) {
            Ok(args) => {
                let base = args.recipe.base.clone();
                let fails = base == "missing";
                let line = format!("pulling {base}");
                let reference = if base == "hang" {
                    "hang".into()
                } else {
                    args.reference
                };
                (ImageJobKind::Build, reference, line, fails)
            }
            Err(refused) => return refused,
        },
        "pull" => match body::<ImagePullArgs>(&request) {
            Ok(args) => {
                let line = format!("pulling {}", args.reference);
                let fails = args.reference == "missing";
                (ImageJobKind::Pull, args.reference, line, fails)
            }
            Err(refused) => return refused,
        },
        "push" => match body::<ImagePushArgs>(&request) {
            Ok(args) => {
                let auth = match args.auth {
                    None => "anonymous",
                    Some(RegistryAuth::Basic { .. }) => "basic",
                    Some(RegistryAuth::Bearer { .. }) => "bearer",
                };
                let line = format!("pushing {} as {auth}", args.reference);
                (ImageJobKind::Push, args.reference, line, false)
            }
            Err(refused) => return refused,
        },
        "commit" => match body::<ImageCommitArgs>(&request) {
            Ok(args) => {
                let fails = !shared.lock().unwrap().sandboxes.contains_key(&args.sandbox);
                let line = format!("committing {}", args.sandbox);
                (ImageJobKind::Commit, args.reference, line, fails)
            }
            Err(refused) => return refused,
        },
        _ => return not_found("such job kind"),
    };
    let mut data = shared.lock().unwrap();
    let id = data.id("job");
    let state = Arc::new(ImageJobState {
        job: Mutex::new(ImageJob {
            id: id.clone(),
            kind,
            reference: reference.clone(),
            running: true,
            digest: None,
            error: None,
        }),
        log: Log::new(),
    });
    state
        .log
        .push(ImageJobEntry::Log(line), ImageJobEntry::Truncated {});
    if fails {
        state.end(ImageJobEnd {
            digest: None,
            error: Some(format!("no image {reference}")),
        });
    } else if reference != "hang" {
        state.end(ImageJobEnd {
            digest: Some("sha256:0".into()),
            error: None,
        });
    }
    data.image_jobs.insert(id.clone(), Arc::clone(&state));
    let job = state.job.lock().unwrap().clone();
    created(format!("/images/jobs/{id}"), &job)
}

async fn get_image_job(State(shared): State<Shared>, Path(id): Path<String>) -> Response {
    match shared.lock().unwrap().image_jobs.get(&id) {
        Some(state) => json(&*state.job.lock().unwrap()),
        None => not_found("such job"),
    }
}

async fn remove_image_job(State(shared): State<Shared>, Path(id): Path<String>) -> Response {
    let Some(state) = shared.lock().unwrap().image_jobs.remove(&id) else {
        return not_found("such job");
    };
    state.end(ImageJobEnd {
        digest: None,
        error: Some("cancelled".into()),
    });
    state.log.remove();
    StatusCode::NO_CONTENT.into_response()
}

async fn image_job_events(
    State(shared): State<Shared>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let log = match shared.lock().unwrap().image_jobs.get(&id) {
        Some(state) => Arc::clone(&state.log),
        None => return not_found("such job"),
    };
    events(log, &headers)
}

async fn prune() -> Response {
    json(&ImagePruneOutput {
        images: vec!["sha256:0".into()],
        reclaimed: 1024,
    })
}

fn volume(name: &str) -> VolumeListOutputItem {
    VolumeListOutputItem {
        name: name.to_owned(),
        size: 0,
    }
}

async fn list_volumes(State(shared): State<Shared>) -> Response {
    let volumes: Vec<_> = shared
        .lock()
        .unwrap()
        .volumes
        .keys()
        .map(|name| volume(name))
        .collect();
    json(&volumes)
}

async fn create_volume(State(shared): State<Shared>, request: Bytes) -> Response {
    let args = match body::<VolumeCreateArgs>(&request) {
        Ok(args) => args,
        Err(refused) => return refused,
    };
    let mut data = shared.lock().unwrap();
    let name = match args.name {
        Some(name) => name,
        None => data.id("volume-"),
    };
    if data.volumes.contains_key(&name) {
        return refuse(
            StatusCode::CONFLICT,
            "exists",
            format!("volume `{name}` exists"),
        );
    }
    let path = format!("/volumes/{}", crate::transport::segment(&name));
    // dav-server strips the prefix from the path it has decoded.
    data.volumes.insert(
        name.clone(),
        (dav(&format!("/volumes/{name}/files")), BTreeMap::new()),
    );
    created(path, &volume(&name))
}

async fn get_volume(State(shared): State<Shared>, Path(name): Path<String>) -> Response {
    if shared.lock().unwrap().volumes.contains_key(&name) {
        json(&volume(&name))
    } else {
        not_found("such volume")
    }
}

async fn remove_volume(State(shared): State<Shared>, Path(name): Path<String>) -> Response {
    match shared.lock().unwrap().volumes.remove(&name) {
        Some(_) => StatusCode::NO_CONTENT.into_response(),
        None => not_found("such volume"),
    }
}

async fn list_mounts(State(shared): State<Shared>, Path(name): Path<String>) -> Response {
    match shared.lock().unwrap().volumes.get(&name) {
        Some((_, mounts)) => json(&mounts.values().collect::<Vec<_>>()),
        None => not_found("such volume"),
    }
}

async fn create_mount(
    State(shared): State<Shared>,
    Path((name, path)): Path<(String, String)>,
    request: Bytes,
) -> Response {
    let args = match body::<VolumeMountCreateArgs>(&request) {
        Ok(args) => args,
        Err(refused) => return refused,
    };
    let mut data = shared.lock().unwrap();
    let Some((_, mounts)) = data.volumes.get_mut(&name) else {
        return not_found("such volume");
    };
    if mounts.contains_key(&path) {
        return refuse(
            StatusCode::CONFLICT,
            "exists",
            format!("a mount at `{path}`"),
        );
    }
    let mount = VolumeMountListOutputItem {
        path: path.clone(),
        mount_type: args.source.mount_type(),
    };
    mounts.insert(path.clone(), mount.clone());
    let location = format!(
        "/volumes/{}/mounts/{}",
        crate::transport::segment(&name),
        crate::transport::segments(&path)
    );
    created(location, &mount)
}

async fn remove_mount(
    State(shared): State<Shared>,
    Path((name, path)): Path<(String, String)>,
) -> Response {
    let mut data = shared.lock().unwrap();
    let Some((_, mounts)) = data.volumes.get_mut(&name) else {
        return not_found("such volume");
    };
    match mounts.remove(&path) {
        Some(_) => StatusCode::NO_CONTENT.into_response(),
        None => not_found("such mount"),
    }
}

/// The name a request under `/{kind}/{name}/files/` names.
fn root_name(request: &axum::extract::Request) -> String {
    let name = request.uri().path().split('/').nth(2).unwrap_or("");
    percent_encoding::percent_decode_str(name)
        .decode_utf8_lossy()
        .into_owned()
}

/// Answers a request under `/volumes/{name}/files/` from that volume's filesystem.
async fn volume_files(State(shared): State<Shared>, request: axum::extract::Request) -> Response {
    let name = root_name(&request);
    let dav = shared
        .lock()
        .unwrap()
        .volumes
        .get(&name)
        .map(|(dav, _)| dav.clone());
    match dav {
        Some(dav) => dav.handle(request).await.map(axum::body::Body::new),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn list_sandboxes(
    State(shared): State<Shared>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
) -> Response {
    // `?name=`, the one query it takes, percent-decoded.
    let name = query.as_deref().and_then(|q| {
        q.split('&').find_map(|pair| {
            let value = pair.strip_prefix("name=")?;
            Some(
                percent_encoding::percent_decode_str(value)
                    .decode_utf8_lossy()
                    .into_owned(),
            )
        })
    });
    let sandboxes: Vec<_> = shared
        .lock()
        .unwrap()
        .sandboxes
        .values()
        .map(|held| held.sandbox.clone())
        .filter(|s| name.is_none() || s.name == name)
        .collect();
    json(&sandboxes)
}

async fn create_sandbox(State(shared): State<Shared>, request: Bytes) -> Response {
    let args = match body::<SandboxCreateArgs>(&request) {
        Ok(args) => args,
        Err(refused) => return refused,
    };
    if args.disk_gib == Some(0) {
        return refuse(StatusCode::BAD_REQUEST, "invalid", "a disk of 0 GiB");
    }
    let mut data = shared.lock().unwrap();
    if let Some(name) = &args.name
        && data
            .sandboxes
            .values()
            .any(|held| held.sandbox.name.as_ref() == Some(name))
    {
        return refuse(
            StatusCode::CONFLICT,
            "exists",
            format!("a sandbox is named `{name}`"),
        );
    }
    let id = data.id("sb");
    let image = match args.image {
        ImageName::Digest { digest } => digest,
        ImageName::Tag { .. } => "sha256:0".into(),
    };
    let sandbox = Sandbox {
        id: id.clone(),
        name: args.name,
        image,
        state: SandboxState::Running,
        ephemeral: args.ephemeral,
        auto_stop_minutes: args.auto_stop_minutes.unwrap_or(15),
        auto_delete_days: args.auto_delete_days.unwrap_or(Some(30)),
        used_at: "2026-01-01T00:00:00Z".into(),
        ports: Vec::new(),
    };
    let ports: Vec<_> = args
        .ports
        .iter()
        .map(|spec| SandboxPort {
            port: spec.port,
            url: format!("http://127.0.0.1:{}", spec.port),
            label: spec.label.clone(),
            protocol: spec.protocol,
        })
        .collect();
    data.sandboxes.insert(
        id.clone(),
        Held {
            sandbox: sandbox.clone(),
            ports: ports.clone(),
            jobs: BTreeMap::new(),
        },
    );
    created(format!("/sandboxes/{id}"), &Sandbox { ports, ..sandbox })
}

async fn get_sandbox(State(shared): State<Shared>, Path(id): Path<String>) -> Response {
    match shared.lock().unwrap().sandboxes.get(&id) {
        Some(held) => json(&held.sandbox),
        None => not_found("such sandbox"),
    }
}

async fn start_sandbox(State(shared): State<Shared>, Path(id): Path<String>) -> Response {
    let mut data = shared.lock().unwrap();
    let Some(held) = data.sandboxes.get_mut(&id) else {
        return not_found("such sandbox");
    };
    held.sandbox.state = SandboxState::Running;
    json(&Sandbox {
        ports: held.ports.clone(),
        ..held.sandbox.clone()
    })
}

async fn stop_sandbox(State(shared): State<Shared>, Path(id): Path<String>) -> Response {
    let mut data = shared.lock().unwrap();
    let Some(held) = data.sandboxes.get_mut(&id) else {
        return not_found("such sandbox");
    };
    held.stop();
    let stopped = held.sandbox.clone();
    if stopped.ephemeral
        && let Some(held) = data.sandboxes.remove(&id)
    {
        for job in held.jobs.values() {
            job.log.remove();
        }
    }
    json(&stopped)
}

async fn remove_sandbox(State(shared): State<Shared>, Path(id): Path<String>) -> Response {
    let Some(mut held) = shared.lock().unwrap().sandboxes.remove(&id) else {
        return not_found("such sandbox");
    };
    held.stop();
    for job in held.jobs.values() {
        job.log.remove();
    }
    StatusCode::NO_CONTENT.into_response()
}

async fn open_port(
    State(shared): State<Shared>,
    Path((id, port)): Path<(String, u16)>,
    body: axum::body::Bytes,
) -> Response {
    if port == 0 {
        return refuse(StatusCode::BAD_REQUEST, "invalid", "port 0");
    }
    // An empty body, or `null`, as a client before labels sent: TCP, named nothing.
    let asked: crate::SandboxPortOpenArgs = match serde_json::from_slice::<Option<_>>(&body) {
        Ok(asked) => asked.unwrap_or_default(),
        Err(_) if body.is_empty() => Default::default(),
        Err(e) => return refuse(StatusCode::BAD_REQUEST, "invalid", &e.to_string()),
    };
    let mut data = shared.lock().unwrap();
    let Some(held) = data.sandboxes.get_mut(&id) else {
        return not_found("such sandbox");
    };
    if let Some(open) = held.ports.iter_mut().find(|open| open.port == port) {
        if asked.label.is_some() {
            open.label = asked.label;
        }
        return json(open);
    }
    let open = SandboxPort {
        port,
        url: format!("http://127.0.0.1:{port}"),
        label: asked.label,
        protocol: asked.protocol.unwrap_or_default(),
    };
    held.ports.push(open.clone());
    created(format!("/sandboxes/{id}/ports/{port}"), &open)
}

async fn list_sandbox_jobs(State(shared): State<Shared>, Path(id): Path<String>) -> Response {
    match shared.lock().unwrap().sandboxes.get(&id) {
        Some(state) => {
            let jobs: Vec<_> = state
                .jobs
                .values()
                .map(|job| job.job.lock().unwrap().clone())
                .collect();
            json(&jobs)
        }
        None => not_found("such sandbox"),
    }
}

async fn exec(State(shared): State<Shared>, Path(id): Path<String>, request: Bytes) -> Response {
    let args = match body::<SandboxExecArgs>(&request) {
        Ok(args) => args,
        Err(refused) => return refused,
    };
    let Some(program) = args.cmd.first().cloned() else {
        return refuse(StatusCode::BAD_REQUEST, "invalid", "an empty cmd");
    };
    if !matches!(
        program.as_str(),
        "echo" | "fail" | "lines" | "cat" | "sleep"
    ) {
        return refuse(
            StatusCode::UNPROCESSABLE_ENTITY,
            "not_started",
            format!("{program}: command not found"),
        );
    }
    let mut data = shared.lock().unwrap();
    let job_id = data.id("job");
    let pid = u32::try_from(data.next).unwrap_or(u32::MAX);
    let Some(sandbox) = data.sandboxes.get_mut(&id) else {
        return not_found("such sandbox");
    };
    // A job started in a stopped sandbox starts it first.
    sandbox.sandbox.state = SandboxState::Running;
    let state = Arc::new(SandboxJobState {
        job: Mutex::new(SandboxJob {
            id: job_id.clone(),
            kind: SandboxJobKind::Exec,
            pid,
            cmd: args.cmd.clone(),
            running: true,
            code: None,
        }),
        stdin: Mutex::new(args.stdin),
        log: Log::new(),
    });
    let rest = &args.cmd[1..];
    match program.as_str() {
        "echo" => {
            state.write(SandboxJobEntry::Stdout(rest.join(" ")));
            state.end(0);
        }
        "fail" => {
            state.write(SandboxJobEntry::Stderr("failed".into()));
            state.end(1);
        }
        "lines" => {
            let n: usize = rest.first().and_then(|n| n.parse().ok()).unwrap_or(0);
            for i in 1..=n {
                state.write(SandboxJobEntry::Stdout(format!("line {i}")));
            }
            state.end(0);
        }
        _ => {}
    }
    if let Some(timeout) = args.timeout_ms {
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(timeout)).await;
            state.end(137);
        });
    }
    sandbox.jobs.insert(job_id.clone(), Arc::clone(&state));
    let job = state.job.lock().unwrap().clone();
    created(format!("/sandboxes/{id}/jobs/{job_id}"), &job)
}

/// The job `job` in the sandbox `id`, or the refusal it is.
#[allow(
    clippy::result_large_err,
    reason = "the refusal is the response, sent as it is"
)]
fn sandbox_job(shared: &Shared, id: &str, job: &str) -> Result<Arc<SandboxJobState>, Response> {
    let data = shared.lock().unwrap();
    let sandbox = data
        .sandboxes
        .get(id)
        .ok_or_else(|| not_found("such sandbox"))?;
    sandbox
        .jobs
        .get(job)
        .cloned()
        .ok_or_else(|| not_found("such job"))
}

async fn get_sandbox_job(
    State(shared): State<Shared>,
    Path((id, job)): Path<(String, String)>,
) -> Response {
    match sandbox_job(&shared, &id, &job) {
        Ok(state) => json(&*state.job.lock().unwrap()),
        Err(refused) => refused,
    }
}

async fn remove_sandbox_job(
    State(shared): State<Shared>,
    Path((id, job)): Path<(String, String)>,
) -> Response {
    let removed = shared
        .lock()
        .unwrap()
        .sandboxes
        .get_mut(&id)
        .map(|sandbox| sandbox.jobs.remove(&job));
    match removed {
        Some(Some(state)) => {
            state.end(137);
            state.log.remove();
            StatusCode::NO_CONTENT.into_response()
        }
        Some(None) => not_found("such job"),
        None => not_found("such sandbox"),
    }
}

async fn signal(
    State(shared): State<Shared>,
    Path((id, job)): Path<(String, String)>,
    request: Bytes,
) -> Response {
    let args = match body::<SandboxSignalArgs>(&request) {
        Ok(args) => args,
        Err(refused) => return refused,
    };
    let number = match args.signal.as_str() {
        "SIGINT" => 2,
        "SIGKILL" => 9,
        "SIGTERM" => 15,
        other => {
            return refuse(
                StatusCode::BAD_REQUEST,
                "invalid",
                format!("no signal {other}"),
            );
        }
    };
    let state = match sandbox_job(&shared, &id, &job) {
        Ok(state) => state,
        Err(refused) => return refused,
    };
    if !state.job.lock().unwrap().running {
        return refuse(StatusCode::CONFLICT, "ended", "the job has ended");
    }
    state.end(128 + number);
    StatusCode::NO_CONTENT.into_response()
}

async fn stdin(
    State(shared): State<Shared>,
    Path((id, job)): Path<(String, String)>,
    request: Bytes,
) -> Response {
    let args = match body::<SandboxStdinArgs>(&request) {
        Ok(args) => args,
        Err(refused) => return refused,
    };
    let state = match sandbox_job(&shared, &id, &job) {
        Ok(state) => state,
        Err(refused) => return refused,
    };
    if !*state.stdin.lock().unwrap() {
        return refuse(StatusCode::CONFLICT, "ended", "its stdin is closed");
    }
    // `cat`: each line written comes back on stdout; `sleep` reads nothing.
    let cat = state.job.lock().unwrap().cmd[0] == "cat";
    if cat {
        for line in args.data.as_deref().unwrap_or("").lines() {
            state.write(SandboxJobEntry::Stdout(line.to_owned()));
        }
    }
    if args.eof {
        *state.stdin.lock().unwrap() = false;
        if cat {
            state.end(0);
        }
    }
    StatusCode::NO_CONTENT.into_response()
}

async fn sandbox_job_events(
    State(shared): State<Shared>,
    Path((id, job)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    match sandbox_job(&shared, &id, &job) {
        Ok(state) => events(Arc::clone(&state.log), &headers),
        Err(refused) => refused,
    }
}
