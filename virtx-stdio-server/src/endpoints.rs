//! The endpoints `virtx-core`'s `PROTOCOL.md` names, but the images' registry, answered from this
//! host's [`Home`]: one client is on the other end of stdio, so all of it is its.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::convert::Infallible;
use std::ffi::OsString;
use std::fmt::Display;
use std::future::Future;
use std::hash::{BuildHasher as _, Hasher as _, RandomState};
use std::net::Ipv4Addr;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Extension, Path as Segments};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post, put};
use http::{HeaderMap, StatusCode, header};
use serde::{Serialize, de::DeserializeOwned};
use tokio::net::TcpListener;
use tokio::sync::{oneshot, watch};
use tokio::task::{AbortHandle, JoinSet};

use virtx_core::{
    ImageBuildArgs, ImageJob, ImageJobEnd, ImageJobEntry, ImageJobKind, ImageName,
    ImagePruneOutput, ImagePullArgs, ImagePushArgs, SandboxCreateArgs, SandboxExecArgs, SandboxJob,
    SandboxJobEnd, SandboxJobEntry, SandboxJobKind, SandboxPort, SandboxSignalArgs,
    SnapshotCreateArgs, SnapshotListOutputItem, VolumeCreateArgs, VolumeListOutputItem,
    VolumeMountCreateArgs, VolumeMountListOutputItem,
};
use virtx_uvm::{
    Digest, Files, ImageConfiguration, ImageHold, ImageStore, ImageStoreError, Machine,
    MachineConfig, MachineError, MachineSpec, Network, Os, Ports, Reference, RegistryClient,
    Snapshot, SnapshotError, SnapshotStore, VolumeError, VolumeMount, VolumeStore, build_step,
    wire::MachineMessage,
};

/// A machine's size where a sandbox leaves it to the server.
const DEFAULT_VCPUS: u8 = 4;
const DEFAULT_MEMORY_MIB: u32 = 4096;

/// How old what nothing needs has to be before a prune takes it: a pull, push or build in
/// flight has put it there and not yet pointed a reference at it.
const PRUNE_GRACE: Duration = Duration::from_secs(60 * 60);

/// How many entries a job's log keeps before its end.
const LOG_LIMIT: usize = 100_000;

/// The signals a job is sent, by name, each numbered as Linux numbers it: its index, plus one.
const SIGNALS: [&str; 31] = [
    "SIGHUP",
    "SIGINT",
    "SIGQUIT",
    "SIGILL",
    "SIGTRAP",
    "SIGABRT",
    "SIGBUS",
    "SIGFPE",
    "SIGKILL",
    "SIGUSR1",
    "SIGSEGV",
    "SIGUSR2",
    "SIGPIPE",
    "SIGALRM",
    "SIGTERM",
    "SIGSTKFLT",
    "SIGCHLD",
    "SIGCONT",
    "SIGSTOP",
    "SIGTSTP",
    "SIGTTIN",
    "SIGTTOU",
    "SIGURG",
    "SIGXCPU",
    "SIGXFSZ",
    "SIGVTALRM",
    "SIGPROF",
    "SIGWINCH",
    "SIGIO",
    "SIGPWR",
    "SIGSYS",
];
const SIGKILL: i32 = 9;

/// What the endpoints answer from: the stores, and the sandboxes and jobs made from them.
pub struct Home {
    images: ImageStore,
    snapshots: SnapshotStore,
    volumes: VolumeStore,
    /// The files of the snapshots and volumes, over WebDAV.
    files: Files,
    /// This binary, as each machine is spawned.
    vm: Vec<OsString>,
    /// The sandboxes, by id, until each is removed.
    sandboxes: Mutex<HashMap<String, Arc<Sandbox>>>,
    /// The image jobs, by id, and what cancels each, until each is removed.
    image_jobs: Mutex<HashMap<String, (Arc<Job<ImageJob, ImageJobEntry>>, AbortHandle)>>,
}

impl Home {
    /// The images in `images`, and the snapshots and volumes under `root`.
    pub fn new(images: ImageStore, root: &Path) -> anyhow::Result<Home> {
        let snapshots = SnapshotStore::new(root)?;
        let volumes = VolumeStore::new(root)?;
        Ok(Home {
            images,
            files: Files::new(snapshots.clone(), volumes.clone()),
            snapshots,
            volumes,
            vm: vec![
                std::env::current_exe()?.into(),
                crate::MACHINE_SUBCOMMAND.into(),
            ],
            sandboxes: Mutex::default(),
            image_jobs: Mutex::default(),
        })
    }

    fn sandbox(&self, id: &str) -> Result<Arc<Sandbox>, Refused> {
        let sandboxes = self.sandboxes.lock().unwrap();
        sandboxes
            .get(id)
            .cloned()
            .ok_or_else(|| not_found(format!("no sandbox here has the id {id}")))
    }

    /// Whether a sandbox here stands on the snapshot `name`.
    fn holds(&self, name: &str) -> bool {
        let sandboxes = self.sandboxes.lock().unwrap();
        sandboxes
            .values()
            .any(|sandbox| sandbox.snapshot.as_deref() == Some(name))
    }

    fn snapshot(&self, snapshot: Snapshot) -> Result<SnapshotListOutputItem, Refused> {
        Ok(SnapshotListOutputItem {
            size: snapshot.size()?,
            in_use: snapshot.in_use()? || self.holds(&snapshot.name),
            name: snapshot.name,
        })
    }
}

/// A sandbox: the machine it booted, up until the sandbox is removed, and the processes run in
/// it.
struct Sandbox {
    /// The image it was made from, as the store names it, and held there for as long as the
    /// sandbox is.
    image: Digest,
    _held: ImageHold,
    snapshot: Option<String>,
    machine: Machine,
    /// Whether its machine runs still.
    up: AtomicBool,
    /// Its jobs, by id, until each is removed.
    jobs: Mutex<BTreeMap<String, Arc<Job<SandboxJob, SandboxJobEntry>>>>,
    /// Each command sent to the machine and not yet answered, in the order sent, and who waits
    /// for its job.
    starting: Mutex<
        VecDeque<(
            Vec<String>,
            oneshot::Sender<Result<Arc<Job<SandboxJob, SandboxJobEntry>>, String>>,
        )>,
    >,
    /// Where its machine's ports are reached, and where it answers each was opened.
    ports: Arc<Ports>,
    opened: Vec<SandboxPort>,
    /// What takes each port's connections, until the sandbox goes.
    _listening: JoinSet<()>,
}

impl Sandbox {
    fn answer(&self, id: String) -> virtx_core::Sandbox {
        virtx_core::Sandbox {
            id,
            image: self.image.to_string(),
            snapshot: self.snapshot.clone(),
            running: self.up.load(Ordering::Relaxed),
            ports: Vec::new(),
        }
    }

    fn job(&self, id: &str) -> Result<Arc<Job<SandboxJob, SandboxJobEntry>>, Refused> {
        let jobs = self.jobs.lock().unwrap();
        jobs.get(id)
            .cloned()
            .ok_or_else(|| not_found(format!("no job here has the id {id}")))
    }

    /// Stop its machine, and end and remove every job in it, as cancelling each does. Blocks
    /// while the guest puts what it wrote on its snapshot.
    fn close(&self) {
        self.machine.stop();
        for job in std::mem::take(&mut *self.jobs.lock().unwrap()).into_values() {
            job.end(
                |job| (job.running, job.code) = (false, Some(128 + SIGKILL)),
                SandboxJobEntry::End(SandboxJobEnd {
                    code: 128 + SIGKILL,
                }),
            );
            job.remove();
        }
    }
}

/// What `sandbox`'s machine says, until it ends: each command's pid, as the job its waiter
/// gets, and each job's output and end.
fn read(sandbox: Arc<Sandbox>) {
    let _published = sandbox.ports.publish(sandbox.machine.published.clone());
    let mut running = HashMap::new();
    while let Some(message) = sandbox.machine.recv() {
        match message {
            MachineMessage::Started(started) => {
                let Some((cmd, waiter)) = sandbox.starting.lock().unwrap().pop_front() else {
                    continue;
                };
                let started = started.map(|pid| {
                    let mut jobs = sandbox.jobs.lock().unwrap();
                    let id = pick(|id| jobs.contains_key(id));
                    let job = Job::new(SandboxJob {
                        id: id.clone(),
                        kind: SandboxJobKind::Exec,
                        pid,
                        cmd,
                        running: true,
                        code: None,
                    });
                    jobs.insert(id, Arc::clone(&job));
                    running.insert(pid, Arc::clone(&job));
                    job
                });
                let _ = waiter.send(started);
            }
            MachineMessage::Stdout { id, line } => {
                if let Some(job) = running.get(&id) {
                    job.write(SandboxJobEntry::Stdout(line), SandboxJobEntry::Truncated {});
                }
            }
            MachineMessage::Stderr { id, line } => {
                if let Some(job) = running.get(&id) {
                    job.write(SandboxJobEntry::Stderr(line), SandboxJobEntry::Truncated {});
                }
            }
            MachineMessage::Exited { id, code } => {
                if let Some(job) = running.remove(&id) {
                    job.end(
                        |job| (job.running, job.code) = (false, Some(code)),
                        SandboxJobEntry::End(SandboxJobEnd { code }),
                    );
                }
            }
            MachineMessage::Ready(_) | MachineMessage::Log { .. } => {}
        }
    }
    // Gone, and what ran in it with it: killed, as far as anything in it can tell.
    let mut starting = sandbox.starting.lock().unwrap();
    sandbox.up.store(false, Ordering::Relaxed);
    starting.clear();
    for job in running.into_values() {
        job.end(
            |job| (job.running, job.code) = (false, Some(128 + SIGKILL)),
            SandboxJobEntry::End(SandboxJobEnd {
                code: 128 + SIGKILL,
            }),
        );
    }
}

/// A job, as it is answered, and its log, kept to [`LOG_LIMIT`] entries before its end.
struct Job<T, E> {
    job: Mutex<T>,
    log: Mutex<Log<E>>,
    changed: watch::Sender<()>,
}

struct Log<E> {
    entries: Vec<E>,
    truncated: bool,
    ended: bool,
    /// The job was removed: a reader's stream ends where it is.
    removed: bool,
}

impl<T: Clone + Serialize + Send + 'static, E: Clone + Serialize + Send + 'static> Job<T, E> {
    fn new(job: T) -> Arc<Self> {
        Arc::new(Job {
            job: Mutex::new(job),
            log: Mutex::new(Log {
                entries: Vec::new(),
                truncated: false,
                ended: false,
                removed: false,
            }),
            changed: watch::channel(()).0,
        })
    }

    /// Keep `entry`, unless the log is past its limit, where `truncated` is kept once instead.
    fn write(&self, entry: E, truncated: E) {
        let mut log = self.log.lock().unwrap();
        if log.ended {
            return;
        }
        if log.entries.len() < LOG_LIMIT {
            log.entries.push(entry);
        } else if !log.truncated {
            log.truncated = true;
            log.entries.push(truncated);
        } else {
            return;
        }
        drop(log);
        self.changed.send_replace(());
    }

    /// End it, as `ended` changes the job, with `end` last in its log; nothing if it has ended.
    fn end(&self, ended: impl FnOnce(&mut T), end: E) {
        let mut log = self.log.lock().unwrap();
        if log.ended {
            return;
        }
        log.ended = true;
        log.entries.push(end);
        ended(&mut self.job.lock().unwrap());
        drop(log);
        self.changed.send_replace(());
    }

    fn remove(&self) {
        self.log.lock().unwrap().removed = true;
        self.changed.send_replace(());
    }

    fn answer(&self) -> Response {
        json(&*self.job.lock().unwrap())
    }

    /// Its log after the entry `Last-Event-ID` in `headers` names, as an event stream: each
    /// entry as it comes, until the end or the job's removal.
    fn events(self: Arc<Self>, headers: &HeaderMap) -> Response {
        let after: usize = headers
            .get("last-event-id")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);
        let changed = self.changed.subscribe();
        let events = futures_util::stream::unfold(
            (self, changed, after),
            |(job, mut changed, next)| async move {
                loop {
                    let (entry, ended) = {
                        let log = job.log.lock().unwrap();
                        if log.removed {
                            return None;
                        }
                        (log.entries.get(next).cloned(), log.ended)
                    };
                    if let Some(entry) = entry {
                        let data = serde_json::to_string(&entry).expect("an entry serializes");
                        let event = Bytes::from(format!("id: {}\ndata: {data}\n\n", next + 1));
                        return Some((Ok::<_, Infallible>(event), (job, changed, next + 1)));
                    }
                    if ended || changed.changed().await.is_err() {
                        return None;
                    }
                }
            },
        );
        (
            [(header::CONTENT_TYPE, "text/event-stream")],
            Body::from_stream(events),
        )
            .into_response()
    }
}

/// Every endpoint but the images' registry, each answered from the [`Home`] in its request's
/// extensions.
pub fn router() -> Router {
    Router::new()
        .route("/images/jobs/", get(image_job_list))
        .route("/images/jobs/pull/", post(image_pull))
        .route("/images/jobs/push/", post(image_push))
        .route("/images/jobs/build/", post(image_build))
        .route(
            "/images/jobs/{id}",
            get(image_job_get).delete(image_job_remove),
        )
        .route("/images/jobs/{id}/events", get(image_job_events))
        .route("/images/prune", post(image_prune))
        .route("/snapshots/", get(snapshot_list).post(snapshot_create))
        .route(
            "/snapshots/{name}",
            get(snapshot_get).delete(snapshot_remove),
        )
        .route("/volumes/", get(volume_list).post(volume_create))
        .route("/volumes/{name}", get(volume_get).delete(volume_remove))
        .route("/volumes/{name}/mounts/", get(volume_mount_list))
        .route(
            "/volumes/{name}/mounts/{*path}",
            put(volume_mount_create).delete(volume_mount_remove),
        )
        .route("/volumes/{name}/files", any(files))
        .route("/volumes/{name}/files/", any(files))
        .route("/volumes/{name}/files/{*path}", any(files))
        .route("/snapshots/{name}/files", any(files))
        .route("/snapshots/{name}/files/", any(files))
        .route("/snapshots/{name}/files/{*path}", any(files))
        .route("/sandboxes/", get(sandbox_list).post(sandbox_create))
        .route("/sandboxes/{id}", get(sandbox_get).delete(sandbox_remove))
        .route("/sandboxes/{id}/jobs/", get(sandbox_job_list))
        .route("/sandboxes/{id}/jobs/exec/", post(sandbox_exec))
        .route(
            "/sandboxes/{id}/jobs/{job}",
            get(sandbox_job_get).delete(sandbox_job_remove),
        )
        .route(
            "/sandboxes/{id}/jobs/{job}/signal",
            post(sandbox_job_signal),
        )
        .route("/sandboxes/{id}/jobs/{job}/stdin", post(sandbox_job_stdin))
        .route("/sandboxes/{id}/jobs/{job}/events", get(sandbox_job_events))
        .method_not_allowed_fallback(async || {
            refused(
                StatusCode::METHOD_NOT_ALLOWED,
                "method_not_allowed",
                "this path takes no such method",
            )
        })
        .fallback(async || not_found("no endpoint here"))
}

type Answer = Result<Response, Refused>;

async fn image_job_list(Extension(home): Extension<Arc<Home>>) -> Answer {
    let jobs = home.image_jobs.lock().unwrap();
    let listed: Vec<_> = jobs
        .values()
        .map(|(job, _)| job.job.lock().unwrap().clone())
        .collect();
    Ok(json(&listed))
}

/// Start a job of `kind` for `reference`: `work`, given what logs a line of it, run until it
/// ends or is cancelled.
fn image_job<Fut>(
    home: &Home,
    kind: ImageJobKind,
    reference: String,
    work: impl FnOnce(Box<dyn FnMut(String) + Send>) -> Fut,
) -> Response
where
    Fut: Future<Output = Result<Digest, Failure>> + Send + 'static,
{
    let mut jobs = home.image_jobs.lock().unwrap();
    let id = pick(|id| jobs.contains_key(id));
    let job = Job::new(ImageJob {
        id: id.clone(),
        kind,
        reference,
        running: true,
        digest: None,
        error: None,
    });
    let logged = Arc::clone(&job);
    let work = work(Box::new(move |line| {
        logged.write(ImageJobEntry::Log(line), ImageJobEntry::Truncated {})
    }));
    let ended = Arc::clone(&job);
    let task = tokio::spawn(async move {
        let end = match work.await {
            Ok(digest) => ImageJobEnd {
                digest: Some(digest.to_string()),
                error: None,
            },
            Err(e) => ImageJobEnd {
                digest: None,
                error: Some(e.to_string()),
            },
        };
        ended.end(
            |job| {
                job.running = false;
                job.digest.clone_from(&end.digest);
                job.error.clone_from(&end.error);
            },
            ImageJobEntry::End(end.clone()),
        );
    });
    jobs.insert(id.clone(), (Arc::clone(&job), task.abort_handle()));
    created(format!("/images/jobs/{id}"), &*job.job.lock().unwrap())
}

async fn image_pull(Extension(home): Extension<Arc<Home>>, request: Bytes) -> Answer {
    let args: ImagePullArgs = body(&request)?;
    let reference = reference(&args.reference)?;
    let images = home.images.clone();
    Ok(image_job(
        &home,
        ImageJobKind::Pull,
        args.reference,
        move |report| async move {
            let registry = RegistryClient::new(images.clone(), args.auth);
            let (digest, image) = registry.pull(&reference, report).await?;
            images.insert_tag(&reference, &image)?;
            Ok(digest)
        },
    ))
}

async fn image_push(Extension(home): Extension<Arc<Home>>, request: Bytes) -> Answer {
    let args: ImagePushArgs = body(&request)?;
    let reference = reference(&args.reference)?;
    let images = home.images.clone();
    Ok(image_job(
        &home,
        ImageJobKind::Push,
        args.reference,
        move |report| async move {
            let registry = RegistryClient::new(images, args.auth);
            Ok(registry.push(&reference, report).await?)
        },
    ))
}

async fn image_build(Extension(home): Extension<Arc<Home>>, request: Bytes) -> Answer {
    let args: ImageBuildArgs = body(&request)?;
    let reference = reference(&args.reference)?;
    if args.recipe.base != "scratch" {
        self::reference(&args.recipe.base)?;
    }
    let (images, vm) = (home.images.clone(), home.vm.clone());
    let recipe = args.recipe;
    Ok(image_job(
        &home,
        ImageJobKind::Build,
        args.reference,
        move |mut report| async move {
            let (mut image, mut layers, _base) = base(&images, &recipe.base, &mut report).await?;
            let steps = recipe.steps.len();
            for (index, step) in recipe.steps.iter().enumerate() {
                report(format!("STEP {}/{steps}: {step}", index + 1));
                build_step(
                    &images,
                    &mut image,
                    &mut layers,
                    step,
                    &vm,
                    &MachineSpec {
                        vcpus: DEFAULT_VCPUS,
                        ram_mib: DEFAULT_MEMORY_MIB,
                        gpu: false,
                        gpu_memory_mib: None,
                        network: Network::Public,
                    },
                    &mut report,
                )
                .await?;
            }
            let digest = images.insert_image(&image, &layers)?;
            images.insert_tag(&reference, &digest)?;
            Ok(digest)
        },
    ))
}

/// A build's base: its config and layers, and its image held for as long as the build runs.
/// Nothing for `scratch`, as in a Dockerfile; else the image tagged `base` here, or the one
/// pulled into the store now, anonymously, and left untagged: a build tags only what it builds.
async fn base(
    images: &ImageStore,
    base: &str,
    report: &mut (impl FnMut(String) + Send),
) -> Result<(ImageConfiguration, Vec<Digest>, Option<ImageHold>), Failure> {
    if base == "scratch" {
        let mut image = ImageConfiguration::default();
        image.set_os(Os::Linux);
        return Ok((image, Vec::new(), None));
    }
    let reference: Reference = base.parse().map_err(|e| format!("base {base}: {e}"))?;
    let image = match images.get_tag(&reference)? {
        Some(digest) if images.get_image(&digest)?.is_some() => digest,
        _ => {
            RegistryClient::new(images.clone(), None)
                .pull(&reference, report)
                .await?
                .1
        }
    };
    let held = images.hold_image(&image)?;
    let (config, layers) = images
        .get_image(&image)?
        .ok_or(ImageStoreError::Missing(image))?;
    Ok((config, layers, Some(held)))
}

async fn image_job_get(
    Extension(home): Extension<Arc<Home>>,
    Segments(id): Segments<String>,
) -> Answer {
    let jobs = home.image_jobs.lock().unwrap();
    match jobs.get(&id) {
        Some((job, _)) => Ok(job.answer()),
        None => Err(not_found(format!("no image job here has the id {id}"))),
    }
}

/// Cancel it if it runs, leaving no tag changed, and remove it.
async fn image_job_remove(
    Extension(home): Extension<Arc<Home>>,
    Segments(id): Segments<String>,
) -> Answer {
    let removed = home.image_jobs.lock().unwrap().remove(&id);
    let (job, task) =
        removed.ok_or_else(|| not_found(format!("no image job here has the id {id}")))?;
    task.abort();
    job.end(
        |job| (job.running, job.error) = (false, Some("cancelled".into())),
        ImageJobEntry::End(ImageJobEnd {
            digest: None,
            error: Some("cancelled".into()),
        }),
    );
    job.remove();
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn image_job_events(
    Extension(home): Extension<Arc<Home>>,
    Segments(id): Segments<String>,
    headers: HeaderMap,
) -> Answer {
    let job = home
        .image_jobs
        .lock()
        .unwrap()
        .get(&id)
        .map(|(job, _)| Arc::clone(job));
    let job = job.ok_or_else(|| not_found(format!("no image job here has the id {id}")))?;
    Ok(job.events(&headers))
}

/// Delete what no reference and no sandbox needs, once it is too old to be what a pull, push
/// or build in flight is putting together.
async fn image_prune(Extension(home): Extension<Arc<Home>>) -> Answer {
    let images = home.images.clone();
    let pruned = tokio::task::spawn_blocking(move || images.prune(PRUNE_GRACE)).await??;
    Ok(json(&ImagePruneOutput {
        images: pruned.images.iter().map(Digest::to_string).collect(),
        reclaimed: pruned.reclaimed,
    }))
}

async fn snapshot_list(Extension(home): Extension<Arc<Home>>) -> Answer {
    let mut listed = Vec::new();
    for snapshot in home.snapshots.list()? {
        listed.push(home.snapshot(snapshot)?);
    }
    Ok(json(&listed))
}

async fn snapshot_create(Extension(home): Extension<Arc<Home>>, request: Bytes) -> Answer {
    let args: SnapshotCreateArgs = body(&request)?;
    if args.disk_gib == Some(0) {
        return Err(invalid("disk_gib: 0 is a disk nothing fits on"));
    }
    let size = args
        .disk_gib
        .map_or(Snapshot::SIZE, |gib| u64::from(gib) << 30);
    // Formatting a disk is blocking work.
    let made = Arc::clone(&home);
    let snapshot = tokio::task::spawn_blocking(move || {
        let name = args
            .name
            .unwrap_or_else(|| pick(|name| made.snapshots.open(name).is_ok()));
        made.snapshots.create(&name, size)
    })
    .await??;
    let location = format!("/snapshots/{}", snapshot.name);
    Ok(created(location, &home.snapshot(snapshot)?))
}

async fn snapshot_get(
    Extension(home): Extension<Arc<Home>>,
    Segments(name): Segments<String>,
) -> Answer {
    Ok(json(&home.snapshot(home.snapshots.open(&name)?)?))
}

async fn snapshot_remove(
    Extension(home): Extension<Arc<Home>>,
    Segments(name): Segments<String>,
) -> Answer {
    // Held from its sandbox's making until its removal, whether its machine runs or not.
    if home.holds(&name) {
        return Err(SnapshotError::InUse(name).into());
    }
    home.snapshots.remove(&name)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

fn volume(home: &Home, name: String) -> Result<VolumeListOutputItem, Refused> {
    Ok(VolumeListOutputItem {
        size: home.volumes.open(&name)?.size(),
        name,
    })
}

async fn volume_list(Extension(home): Extension<Arc<Home>>) -> Answer {
    let mut listed = Vec::new();
    for name in home.volumes.list()? {
        listed.push(volume(&home, name)?);
    }
    Ok(json(&listed))
}

async fn volume_create(Extension(home): Extension<Arc<Home>>, request: Bytes) -> Answer {
    let args: VolumeCreateArgs = body(&request)?;
    let name = args
        .name
        .unwrap_or_else(|| pick(|name| home.volumes.open(name).is_ok()));
    let name = home.volumes.create(&name)?.name().to_owned();
    Ok(created(format!("/volumes/{name}"), &volume(&home, name)?))
}

async fn volume_get(
    Extension(home): Extension<Arc<Home>>,
    Segments(name): Segments<String>,
) -> Answer {
    Ok(json(&volume(&home, name)?))
}

async fn volume_remove(
    Extension(home): Extension<Arc<Home>>,
    Segments(name): Segments<String>,
) -> Answer {
    home.volumes.remove(&name)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn volume_mount_list(
    Extension(home): Extension<Arc<Home>>,
    Segments(name): Segments<String>,
) -> Answer {
    let volume = home.volumes.open(&name)?;
    let listed: Vec<_> = volume
        .mounts()
        .iter()
        .map(|mount| VolumeMountListOutputItem {
            path: mount.path.clone(),
            mount_type: mount.source.mount_type(),
        })
        .collect();
    Ok(json(&listed))
}

async fn volume_mount_create(
    Extension(home): Extension<Arc<Home>>,
    Segments((name, path)): Segments<(String, String)>,
    request: Bytes,
) -> Answer {
    let args: VolumeMountCreateArgs = body(&request)?;
    let mount_type = args.source.mount_type();
    home.volumes.open(&name)?.mount(path.clone(), args.source)?;
    let location = format!("/volumes/{name}/mounts/{path}");
    Ok(created(
        location,
        &VolumeMountListOutputItem { path, mount_type },
    ))
}

async fn volume_mount_remove(
    Extension(home): Extension<Arc<Home>>,
    Segments((name, path)): Segments<(String, String)>,
) -> Answer {
    home.volumes.open(&name)?.unmount(&path)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// A request to the files of a volume or a snapshot, over WebDAV: a snapshot a sandbox stands
/// on answers `423`.
async fn files(Extension(home): Extension<Arc<Home>>, request: axum::extract::Request) -> Response {
    home.files.handle(request, &|name| home.holds(name)).await
}

async fn sandbox_list(Extension(home): Extension<Arc<Home>>) -> Answer {
    let sandboxes = home.sandboxes.lock().unwrap();
    let listed: Vec<_> = sandboxes
        .iter()
        .map(|(id, sandbox)| sandbox.answer(id.clone()))
        .collect();
    Ok(json(&listed))
}

async fn sandbox_create(Extension(home): Extension<Arc<Home>>, request: Bytes) -> Answer {
    let args: SandboxCreateArgs = body(&request)?;
    // What can be refused without starting anything, first.
    let mut given = HashSet::new();
    if let Some(port) = args
        .ports
        .iter()
        .find(|port| **port == 0 || !given.insert(**port))
    {
        return Err(invalid(format!(
            "ports: {port} is not a port, or is given twice"
        )));
    }
    for (what, given) in [
        ("vcpus", args.vcpus.map(u32::from)),
        ("memory_mib", args.memory_mib),
        ("gpu_memory_mib", args.gpu_memory_mib),
    ] {
        if given == Some(0) {
            return Err(invalid(format!("{what}: 0 is a machine nothing runs on")));
        }
    }
    let gpu = args.gpu.unwrap_or(false);
    if gpu && !cfg!(feature = "gpu") {
        return Err(refused(
            StatusCode::UNPROCESSABLE_ENTITY,
            "unsupported",
            "this server was built without a GPU",
        ));
    }
    if args.gpu_memory_mib.is_some() && !gpu {
        return Err(invalid(
            "gpu_memory_mib: a size is of a GPU, and this sandbox has none",
        ));
    }

    let digest = match &args.image {
        ImageName::Tag { tag } => {
            let reference = reference(tag)?;
            home.images
                .get_tag(&reference)?
                .ok_or_else(|| not_found(format!("no image here is tagged {tag}")))?
        }
        ImageName::Digest { digest } => {
            let digest = digest
                .parse::<Digest>()
                .map_err(|e| invalid(format!("{digest}: {e}")))?;
            // A registry's digest, as a pull or a push answers, names the image it copied.
            home.images.get_image_origin(&digest)?.unwrap_or(digest)
        }
    };
    let missing = || not_found(format!("no image here is {digest}"));
    let held = home.images.hold_image(&digest).map_err(|e| match e {
        ImageStoreError::Missing(_) => missing(),
        e => e.into(),
    })?;
    let (image, layers) = home.images.get_image(&digest)?.ok_or_else(missing)?;

    let snapshot = args
        .snapshot
        .as_deref()
        .map(|name| home.snapshots.open(name))
        .transpose()?;
    if let Some(snapshot) = &snapshot
        && (snapshot.in_use()? || home.holds(&snapshot.name))
    {
        return Err(SnapshotError::InUse(snapshot.name.clone()).into());
    }
    let volumes = args
        .volumes
        .iter()
        .map(|asked| {
            Ok(VolumeMount {
                path: home.volumes.open(&asked.volume)?.path().to_owned(),
                at: asked.path.clone(),
                read_only: asked.readonly,
            })
        })
        .collect::<Result<_, Refused>>()?;

    // Merging the layers' metadata is blocking work.
    let images = home.images.clone();
    let rootfs = tokio::task::spawn_blocking(move || images.insert_rootfs(&layers)).await??;

    let config = MachineConfig {
        rootfs,
        config: image.config().clone().unwrap_or_default(),
        snapshot,
        spec: MachineSpec {
            vcpus: args.vcpus.unwrap_or(DEFAULT_VCPUS),
            ram_mib: args.memory_mib.unwrap_or(DEFAULT_MEMORY_MIB),
            gpu,
            gpu_memory_mib: args.gpu_memory_mib,
            network: match args.network {
                true => Network::Public,
                false => Network::None,
            },
        },
        volumes,
        ports: args.ports.clone(),
    };

    // Each port on this host's loopback, which only this machine reaches, taken to the
    // sandbox's machine while it runs.
    let ports = Arc::new(Ports::default());
    let mut opened = Vec::new();
    let mut listening = JoinSet::new();
    for &port in &args.ports {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .map_err(|e| internal(format!("opening port {port}: {e}")))?;
        let at = listener
            .local_addr()
            .map_err(|e| internal(format!("opening port {port}: {e}")))?;
        opened.push(SandboxPort {
            port,
            url: format!("http://{at}"),
        });
        let ports = Arc::clone(&ports);
        listening.spawn(async move {
            while let Ok((mut accepted, _)) = listener.accept().await {
                let ports = Arc::clone(&ports);
                tokio::spawn(async move {
                    // Closed at once when the machine has ended, as a port nothing listens on is.
                    if let Ok(mut reached) = ports.connect(port).await {
                        let _ = tokio::io::copy_bidirectional(&mut accepted, &mut reached).await;
                    }
                });
            }
        });
    }

    // Booting is blocking work, until the guest takes commands.
    let vm = home.vm.clone();
    let machine = tokio::task::spawn_blocking(move || Machine::start(config, &vm)).await??;
    let sandbox = Arc::new(Sandbox {
        image: digest,
        _held: held,
        snapshot: args.snapshot,
        machine,
        up: AtomicBool::new(true),
        jobs: Mutex::default(),
        starting: Mutex::default(),
        ports,
        opened,
        _listening: listening,
    });
    let id = {
        let mut sandboxes = home.sandboxes.lock().unwrap();
        let id = pick(|id| sandboxes.contains_key(id));
        sandboxes.insert(id.clone(), Arc::clone(&sandbox));
        id
    };
    let reading = Arc::clone(&sandbox);
    std::thread::spawn(move || read(reading));
    let answer = virtx_core::Sandbox {
        ports: sandbox.opened.clone(),
        ..sandbox.answer(id.clone())
    };
    Ok(created(format!("/sandboxes/{id}"), &answer))
}

async fn sandbox_get(
    Extension(home): Extension<Arc<Home>>,
    Segments(id): Segments<String>,
) -> Answer {
    Ok(json(&home.sandbox(&id)?.answer(id)))
}

async fn sandbox_remove(
    Extension(home): Extension<Arc<Home>>,
    Segments(id): Segments<String>,
) -> Answer {
    let removed = home.sandboxes.lock().unwrap().remove(&id);
    let removed = removed.ok_or_else(|| not_found(format!("no sandbox here has the id {id}")))?;
    tokio::task::spawn_blocking(move || removed.close()).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn sandbox_job_list(
    Extension(home): Extension<Arc<Home>>,
    Segments(id): Segments<String>,
) -> Answer {
    let sandbox = home.sandbox(&id)?;
    let jobs = sandbox.jobs.lock().unwrap();
    let listed: Vec<_> = jobs
        .values()
        .map(|job| job.job.lock().unwrap().clone())
        .collect();
    Ok(json(&listed))
}

async fn sandbox_exec(
    Extension(home): Extension<Arc<Home>>,
    Segments(id): Segments<String>,
    request: Bytes,
) -> Answer {
    let args: SandboxExecArgs = body(&request)?;
    if args.cmd.is_empty() {
        return Err(invalid("cmd: an exec names a command"));
    }
    if args.stdin {
        return Err(refused(
            StatusCode::UNPROCESSABLE_ENTITY,
            "not_started",
            "stdin: this server gives a process none to write to",
        ));
    }
    let sandbox = home.sandbox(&id)?;
    let ended = || {
        refused(
            StatusCode::CONFLICT,
            "ended",
            format!("sandbox {id}'s machine has exited"),
        )
    };
    let (waiter, started) = oneshot::channel();
    {
        let mut starting = sandbox.starting.lock().unwrap();
        if !sandbox.up.load(Ordering::Relaxed) {
            return Err(ended());
        }
        sandbox.machine.exec(args.cmd.clone())?;
        starting.push_back((args.cmd, waiter));
    }
    let job = started
        .await
        .map_err(|_| ended())?
        .map_err(|why| refused(StatusCode::UNPROCESSABLE_ENTITY, "not_started", why))?;
    if let Some(timeout) = args.timeout_ms {
        let (sandbox, job) = (Arc::downgrade(&sandbox), Arc::clone(&job));
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(timeout)).await;
            let (pid, running) = {
                let job = job.job.lock().unwrap();
                (job.pid, job.running)
            };
            if let Some(sandbox) = sandbox.upgrade()
                && running
            {
                let _ = sandbox.machine.signal(pid, SIGKILL);
            }
        });
    }
    let location = format!("/sandboxes/{id}/jobs/{}", job.job.lock().unwrap().id);
    Ok(created(location, &*job.job.lock().unwrap()))
}

async fn sandbox_job_get(
    Extension(home): Extension<Arc<Home>>,
    Segments((id, job)): Segments<(String, String)>,
) -> Answer {
    Ok(home.sandbox(&id)?.job(&job)?.answer())
}

/// Kill it if it runs, and remove it.
async fn sandbox_job_remove(
    Extension(home): Extension<Arc<Home>>,
    Segments((id, job)): Segments<(String, String)>,
) -> Answer {
    let sandbox = home.sandbox(&id)?;
    let removed = sandbox.jobs.lock().unwrap().remove(&job);
    let job = removed.ok_or_else(|| not_found(format!("no job here has the id {job}")))?;
    let (pid, running) = {
        let job = job.job.lock().unwrap();
        (job.pid, job.running)
    };
    if running {
        sandbox.machine.signal(pid, SIGKILL)?;
    }
    job.remove();
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn sandbox_job_signal(
    Extension(home): Extension<Arc<Home>>,
    Segments((id, job)): Segments<(String, String)>,
    request: Bytes,
) -> Answer {
    let args: SandboxSignalArgs = body(&request)?;
    let signal = SIGNALS
        .iter()
        .position(|name| *name == args.signal)
        .ok_or_else(|| invalid(format!("signal: no signal is named {}", args.signal)))?;
    let sandbox = home.sandbox(&id)?;
    let (pid, running) = {
        let job = sandbox.job(&job)?;
        let job = job.job.lock().unwrap();
        (job.pid, job.running)
    };
    if !running {
        return Err(refused(StatusCode::CONFLICT, "ended", "the job has ended"));
    }
    sandbox.machine.signal(pid, signal as i32 + 1)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// Refused for every job: none is started with a stdin to write to.
async fn sandbox_job_stdin(
    Extension(home): Extension<Arc<Home>>,
    Segments((id, job)): Segments<(String, String)>,
) -> Answer {
    home.sandbox(&id)?.job(&job)?;
    Err(refused(
        StatusCode::CONFLICT,
        "ended",
        "the job was started without stdin",
    ))
}

async fn sandbox_job_events(
    Extension(home): Extension<Arc<Home>>,
    Segments((id, job)): Segments<(String, String)>,
    headers: HeaderMap,
) -> Answer {
    Ok(home.sandbox(&id)?.job(&job)?.events(&headers))
}

/// A name for what a client named nothing: eight hex digits for which `taken` is false.
fn pick(taken: impl Fn(&str) -> bool) -> String {
    loop {
        let name = format!("{:08x}", RandomState::new().build_hasher().finish() as u32);
        if !taken(&name) {
            return name;
        }
    }
}

fn reference(reference: &str) -> Result<Reference, Refused> {
    reference
        .parse()
        .map_err(|e| invalid(format!("{reference}: {e}")))
}

/// `request` read as a `T`.
fn body<T: DeserializeOwned>(request: &Bytes) -> Result<T, Refused> {
    serde_json::from_slice(request).map_err(invalid)
}

/// `value` as a JSON body.
fn json(value: &impl Serialize) -> Response {
    let body = serde_json::to_vec(value).expect("an answer serializes");
    ([(header::CONTENT_TYPE, "application/json")], body).into_response()
}

/// `value`, made at `location`.
fn created(location: String, value: &impl Serialize) -> Response {
    (
        StatusCode::CREATED,
        [(header::LOCATION, location)],
        json(value),
    )
        .into_response()
}

/// Why an image job failed, for a person.
type Failure = Box<dyn std::error::Error + Send + Sync>;

/// A request's refusal: its status, and the `code` and `message` of its body.
pub struct Refused(StatusCode, &'static str, String);

pub fn refused(status: StatusCode, code: &'static str, message: impl Display) -> Refused {
    Refused(status, code, message.to_string())
}

fn invalid(message: impl Display) -> Refused {
    refused(StatusCode::BAD_REQUEST, "invalid", message)
}

fn not_found(message: impl Display) -> Refused {
    refused(StatusCode::NOT_FOUND, "not_found", message)
}

fn internal(message: impl Display) -> Refused {
    refused(StatusCode::INTERNAL_SERVER_ERROR, "internal", message)
}

impl IntoResponse for Refused {
    fn into_response(self) -> Response {
        let Refused(status, code, message) = self;
        let body = serde_json::json!({"error": {"code": code, "message": message}});
        (status, json(&body)).into_response()
    }
}

/// The refusal an operation on the host's files that failed with `e` is.
fn io_refused(e: &std::io::Error, message: impl Display) -> Refused {
    match e.kind() {
        std::io::ErrorKind::NotFound => not_found(message),
        std::io::ErrorKind::AlreadyExists => refused(StatusCode::CONFLICT, "exists", message),
        _ => internal(message),
    }
}

impl From<SnapshotError> for Refused {
    fn from(e: SnapshotError) -> Refused {
        match &e {
            SnapshotError::InvalidName(_) => invalid(e),
            SnapshotError::Unknown(_) => not_found(e),
            SnapshotError::AlreadyExists(_) => refused(StatusCode::CONFLICT, "exists", e),
            SnapshotError::InUse(_) => refused(StatusCode::CONFLICT, "in_use", e),
            SnapshotError::Io { source, .. } => io_refused(source, &e),
            SnapshotError::Fstree(_) => internal(e),
        }
    }
}

impl From<VolumeError> for Refused {
    fn from(e: VolumeError) -> Refused {
        match &e {
            VolumeError::InvalidName(_)
            | VolumeError::InvalidPath(_)
            | VolumeError::Refused(_)
            | VolumeError::Source(_) => invalid(e),
            VolumeError::Unknown(_) | VolumeError::NoMount { .. } => not_found(e),
            VolumeError::AlreadyExists(_) => refused(StatusCode::CONFLICT, "exists", e),
            VolumeError::InUse(_) => refused(StatusCode::CONFLICT, "in_use", e),
            VolumeError::Io { source, .. } => io_refused(source, &e),
            VolumeError::Origins(..) => internal(e),
        }
    }
}

impl From<ImageStoreError> for Refused {
    fn from(e: ImageStoreError) -> Refused {
        match &e {
            ImageStoreError::Missing(_) => not_found(e),
            ImageStoreError::InUse(..) => refused(StatusCode::CONFLICT, "in_use", e),
            ImageStoreError::Io { source, .. } => io_refused(source, &e),
            ImageStoreError::Fstree(_) | ImageStoreError::Malformed(_) => internal(e),
        }
    }
}

/// What booting a sandbox's machine, or sending it a command or a signal, fails with.
impl From<MachineError> for Refused {
    fn from(e: MachineError) -> Refused {
        match &e {
            MachineError::NoCommand => invalid(e),
            MachineError::DidNotStart(_) => {
                refused(StatusCode::INTERNAL_SERVER_ERROR, "boot_failed", e)
            }
            _ => internal(e),
        }
    }
}

impl From<tokio::task::JoinError> for Refused {
    fn from(e: tokio::task::JoinError) -> Refused {
        internal(e)
    }
}
