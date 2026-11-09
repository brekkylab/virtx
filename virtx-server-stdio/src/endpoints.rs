//! The endpoints `virtx-core`'s `PROTOCOL.md` names, but the images' registry, answered from this
//! host's [`Home`]: one client is on the other end of stdio, so all of it is its.
//!
//! A sandbox's disk is a snapshot of the store's, named by its id, and what lasts past its
//! machine is kept on disk beside it ([`Sandboxes`]): a server spawned again knows each,
//! stopped. Its ports are on this host's loopback, each at an address this server picks once
//! it first starts or opens it ([`Loopback`]).

use std::collections::HashMap;
use std::ffi::OsString;
use std::fmt::Display;
use std::future::Future;
use std::hash::{BuildHasher as _, Hasher as _, RandomState};
use std::net::Ipv4Addr;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Extension, Path as Segments, RawQuery};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post, put};
use futures_util::future::BoxFuture;
use http::{HeaderMap, StatusCode, header};
use serde::{Serialize, de::DeserializeOwned};
use tokio::net::TcpListener;
use tokio::task::AbortHandle;

use virtx_core::{
    ImageBuildArgs, ImageCommitArgs, ImageJob, ImageJobEnd, ImageJobEntry, ImageJobKind,
    ImagePruneOutput, ImagePullArgs, ImagePushArgs, Log, SandboxCreateArgs, SandboxExecArgs,
    SandboxPort, SandboxPortOpenArgs, SandboxSignalArgs, SandboxStdinArgs, VolumeCreateArgs,
    VolumeListOutputItem, VolumeMountCreateArgs, VolumeMountListOutputItem,
};
use virtx_uvm::sandbox::{Held, Hooks};
use virtx_uvm::{
    DEFAULT_MEMORY_MIB, DEFAULT_VCPUS, Digest, Files, ImageConfiguration, ImageHold, ImageStore,
    ImageStoreError, LOG_LIMIT, MachineError, MachineSpec, Network, Os, Reference, RegistryClient,
    Sandbox, SandboxError, Sandboxes, SnapshotError, SnapshotStore, VolumeError, VolumeStore,
    build_step, signal_number,
};

/// An image job, with the entries its log carries.
type ImageJobOf = Job<ImageJob, ImageJobEntry>;

/// How old what nothing needs has to be before a prune takes it: a pull, push or build in
/// flight has put it there and not yet pointed a reference at it.
const PRUNE_GRACE: Duration = Duration::from_secs(60 * 60);

/// The header an exec answered with its log carries its job in, as JSON.
const JOB_HEADER: &str = "virtx-job";

/// What the endpoints answer from: the stores, and the sandboxes and jobs made from them.
pub struct Home {
    images: ImageStore,
    volumes: VolumeStore,
    /// The files of the volumes, over WebDAV, and of the sandboxes, through their machines.
    files: Files,
    /// This binary, as each machine is spawned.
    vm: Vec<OsString>,
    sandboxes: Sandboxes,
    /// The image jobs, by id, and what cancels each, until each is removed.
    image_jobs: Mutex<HashMap<String, (Arc<ImageJobOf>, AbortHandle)>>,
}

impl Home {
    /// The images in `images`, and the volumes, sandboxes' disks and records under `root`.
    pub fn new(images: ImageStore, root: &Path) -> anyhow::Result<Home> {
        let snapshots = SnapshotStore::new(root)?;
        let volumes = VolumeStore::new(root)?;
        let vm: Vec<OsString> = vec![
            std::env::current_exe()?.into(),
            crate::MACHINE_SUBCOMMAND.into(),
        ];
        let sandboxes = Sandboxes::new(
            images.clone(),
            snapshots.clone(),
            volumes.clone(),
            root,
            vm.clone(),
            Arc::new(Loopback),
        )?;
        Ok(Home {
            images,
            files: Files::new(snapshots, volumes.clone()),
            volumes,
            vm,
            sandboxes,
            image_jobs: Mutex::default(),
        })
    }

    /// Its sandboxes, stopped once unused for their `auto_stop_minutes` and deleted once kept
    /// past their `auto_delete_days`, from now on.
    pub fn tend(&self) {
        self.sandboxes.spawn_tending();
    }

    /// Stop every sandbox here, side by side: blocks until each machine has stopped and synced
    /// what it wrote. Each is kept, stopped, for the next server over this home -- an ephemeral
    /// one, deleted.
    pub fn close_all(&self) {
        self.sandboxes.close_all_blocking();
    }
}

/// What answers a sandbox's ports on this host: each on its loopback, which only this machine
/// reaches, at an address picked as the port is first started or opened, and taken to the
/// sandbox's machine while it runs.
struct Loopback;

impl Hooks for Loopback {
    fn listen<'a>(
        &'a self,
        sandbox: &'a Arc<Sandbox>,
        port: &'a SandboxPort,
    ) -> BoxFuture<'a, Result<Option<(String, Held)>, SandboxError>> {
        Box::pin(async move {
            let port = port.port;
            let opening =
                |e: std::io::Error| SandboxError::Failed(format!("opening port {port}: {e}"));
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
                .await
                .map_err(opening)?;
            let at = listener.local_addr().map_err(opening)?;
            let sandbox = Arc::downgrade(sandbox);
            let task = tokio::spawn(async move {
                while let Ok((mut accepted, _)) = listener.accept().await {
                    let Some(sandbox) = sandbox.upgrade() else {
                        return;
                    };
                    let activity = sandbox.activity();
                    tokio::spawn(async move {
                        // Closed at once when no machine runs, as a port nothing listens on is.
                        if let Ok(mut reached) = activity.sandbox().connect(port).await {
                            let _ =
                                tokio::io::copy_bidirectional(&mut accepted, &mut reached).await;
                        }
                    });
                }
            });
            // On this host's loopback, an `http` one is reached as HTTP at its address as is.
            Ok(Some((
                format!("http://{at}"),
                Box::new(Listening(task)) as Held,
            )))
        })
    }
}

/// What takes a port's connections, until it drops.
struct Listening(tokio::task::JoinHandle<()>);

impl Drop for Listening {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// A job, as it is answered, and its log, kept to [`LOG_LIMIT`] entries before its end.
struct Job<T, E> {
    job: Mutex<T>,
    log: Arc<Log<E>>,
}

impl<T: Clone + Serialize + Send + 'static, E: Clone + Serialize + Send + Sync + 'static>
    Job<T, E>
{
    /// `job`, with an empty log that keeps `truncated` once past its limit.
    fn new(job: T, truncated: E) -> Arc<Self> {
        Arc::new(Job {
            job: Mutex::new(job),
            log: Log::new(LOG_LIMIT, truncated),
        })
    }

    fn write(&self, entry: E) {
        self.log.push(entry);
    }

    /// End it, as `ended` changes the job, with `end` last in its log; nothing if it has ended.
    fn end(&self, ended: impl FnOnce(&mut T), end: E) {
        self.log.end(end, || ended(&mut self.job.lock().unwrap()));
    }

    fn remove(&self) {
        self.log.remove();
    }

    fn answer(&self) -> Response {
        json(&*self.job.lock().unwrap())
    }

    fn events(&self, headers: &HeaderMap) -> Response {
        events(&self.log, headers)
    }
}

/// `log` after the entry `Last-Event-ID` in `headers` names, as an event stream.
fn events<E: Clone + Serialize + Send + Sync + 'static>(
    log: &Arc<Log<E>>,
    headers: &HeaderMap,
) -> Response {
    (
        [(header::CONTENT_TYPE, "text/event-stream")],
        Body::from_stream(Arc::clone(log).events(headers)),
    )
        .into_response()
}

/// Every endpoint but the images' registry, each answered from the [`Home`] in its request's
/// extensions.
pub fn router() -> Router {
    Router::new()
        .route("/images/jobs/", get(image_job_list))
        .route("/images/jobs/pull/", post(image_pull))
        .route("/images/jobs/push/", post(image_push))
        .route("/images/jobs/build/", post(image_build))
        .route("/images/jobs/commit/", post(image_commit))
        .route(
            "/images/jobs/{id}",
            get(image_job_get).delete(image_job_remove),
        )
        .route("/images/jobs/{id}/events", get(image_job_events))
        .route("/images/prune", post(image_prune))
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
        .route("/sandboxes/{id}/files", any(sandbox_files))
        .route("/sandboxes/{id}/files/", any(sandbox_files))
        .route("/sandboxes/{id}/files/{*path}", any(sandbox_files))
        .route("/sandboxes/", get(sandbox_list).post(sandbox_create))
        .route("/sandboxes/{id}", get(sandbox_get).delete(sandbox_remove))
        .route("/sandboxes/{id}/start", post(sandbox_start))
        .route("/sandboxes/{id}/stop", post(sandbox_stop))
        .route(
            "/sandboxes/{id}/ports/{port}",
            put(sandbox_port_open).delete(sandbox_port_close),
        )
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
    let job = Job::new(
        ImageJob {
            id: id.clone(),
            kind,
            reference,
            running: true,
            digest: None,
            error: None,
        },
        ImageJobEntry::Truncated {},
    );
    let logged = Arc::clone(&job);
    let work = work(Box::new(move |line| logged.write(ImageJobEntry::Log(line))));
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
            let (_, image) = registry.pull(&reference, report).await?;
            images.insert_tag(&reference, &image)?;
            Ok(image)
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
                let before = layers.len();
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
                let layered = layers.len() > before;
                virtx_uvm::history_kept(&mut image, step.to_string(), layered, layers.len());
            }
            virtx_uvm::recipe_kept(&mut image, &recipe);
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

/// A commit: the image the sandbox stands on, with what it has written as one more layer, and
/// that image's config, tagged `reference`; see [`Sandbox::commit`].
async fn image_commit(Extension(home): Extension<Arc<Home>>, request: Bytes) -> Answer {
    let args: ImageCommitArgs = body(&request)?;
    let reference = reference(&args.reference)?;
    let sandbox = home.sandboxes.get(&args.sandbox)?;
    Ok(image_job(
        &home,
        ImageJobKind::Commit,
        args.reference,
        move |mut report| async move { Ok(sandbox.commit(&reference, &mut *report).await?) },
    ))
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

/// A request to the files of a volume, over WebDAV.
async fn files(Extension(home): Extension<Arc<Home>>, request: axum::extract::Request) -> Response {
    home.files.handle(request, &|_| false).await
}

/// A request to the files of a sandbox, over WebDAV, asked of its machine while it runs: one
/// that is stopped answers `409`, `stopped`, and is not started for it.
async fn sandbox_files(
    Extension(home): Extension<Arc<Home>>,
    Segments(segments): Segments<HashMap<String, String>>,
    request: axum::extract::Request,
) -> Answer {
    let id = &segments["id"];
    let sandbox = home.sandboxes.get(id)?;
    // In use from here until the answer has been sent.
    let activity = sandbox.activity();
    let machine = sandbox.running_machine()?;
    Ok(home.files.sandbox(id, machine, activity, request).await)
}

/// Every sandbox, running or stopped; `?name=` those named so, one at most.
async fn sandbox_list(Extension(home): Extension<Arc<Home>>, RawQuery(query): RawQuery) -> Answer {
    let name = query.as_deref().and_then(|query| {
        query.split('&').find_map(|pair| {
            let value = pair.strip_prefix("name=")?;
            Some(percent_decode(value).map_err(|e| invalid(format!("name: {e}"))))
        })
    });
    let name = name.transpose()?;
    Ok(json(&home.sandboxes.list(name.as_deref())))
}

/// `value` with each `%XX` decoded, and each `+` a space, as UTF-8.
fn percent_decode(value: &str) -> Result<String, String> {
    let mut bytes = Vec::with_capacity(value.len());
    let mut rest = value.bytes();
    while let Some(b) = rest.next() {
        match b {
            b'%' => {
                let hex = [rest.next(), rest.next()];
                let [Some(high), Some(low)] = hex else {
                    return Err("a % at its end".into());
                };
                let digit = |d: u8| (d as char).to_digit(16).ok_or("not a hex digit");
                bytes.push((digit(high)? * 16 + digit(low)?) as u8);
            }
            b'+' => bytes.push(b' '),
            b => bytes.push(b),
        }
    }
    String::from_utf8(bytes).map_err(|e| e.to_string())
}

async fn sandbox_create(Extension(home): Extension<Arc<Home>>, request: Bytes) -> Answer {
    let args: SandboxCreateArgs = body(&request)?;
    let answer = home.sandboxes.create(args, None, true).await?;
    Ok(created(format!("/sandboxes/{}", answer.id), &answer))
}

async fn sandbox_get(
    Extension(home): Extension<Arc<Home>>,
    Segments(id): Segments<String>,
) -> Answer {
    Ok(json(&home.sandboxes.get(&id)?.answer(false)))
}

/// Started if it is stopped; answered with where its ports are.
async fn sandbox_start(
    Extension(home): Extension<Arc<Home>>,
    Segments(id): Segments<String>,
) -> Answer {
    Ok(json(&home.sandboxes.get(&id)?.start().await?))
}

async fn sandbox_stop(
    Extension(home): Extension<Arc<Home>>,
    Segments(id): Segments<String>,
) -> Answer {
    Ok(json(&home.sandboxes.stop(&id).await?))
}

async fn sandbox_remove(
    Extension(home): Extension<Arc<Home>>,
    Segments(id): Segments<String>,
) -> Answer {
    home.sandboxes.delete(&id).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// Port `port` opened: `201` and where it is reached, or `200` and where, for one open
/// already.
async fn sandbox_port_open(
    Extension(home): Extension<Arc<Home>>,
    Segments((id, port)): Segments<(String, u16)>,
    request: Bytes,
) -> Answer {
    let asked = port_args(&request)?;
    let sandbox = home.sandboxes.get(&id)?;
    match sandbox.open_port(port, asked.label, asked.protocol).await? {
        (open, true) => Ok(created(format!("/sandboxes/{id}/ports/{port}"), &open)),
        (open, false) => Ok(json(&open)),
    }
}

/// What a port's opening asks: none, `null` or `{}` for TCP, named nothing.
fn port_args(request: &Bytes) -> Result<SandboxPortOpenArgs, Refused> {
    match request.iter().all(u8::is_ascii_whitespace) {
        true => Ok(SandboxPortOpenArgs::default()),
        false => Ok(
            serde_json::from_slice::<Option<SandboxPortOpenArgs>>(request)
                .map_err(invalid)?
                .unwrap_or_default(),
        ),
    }
}

async fn sandbox_port_close(
    Extension(home): Extension<Arc<Home>>,
    Segments((id, port)): Segments<(String, u16)>,
) -> Answer {
    home.sandboxes.get(&id)?.close_port(port)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn sandbox_job_list(
    Extension(home): Extension<Arc<Home>>,
    Segments(id): Segments<String>,
) -> Answer {
    Ok(json(&home.sandboxes.get(&id)?.jobs()))
}

/// `POST /sandboxes/{id}/jobs/exec/`, answered with the job -- or, to one that accepts
/// `text/event-stream`, `201` with the job's log as the body, the job in [`JOB_HEADER`] and
/// its path in `Location`: its output and end with no second request.
async fn sandbox_exec(
    Extension(home): Extension<Arc<Home>>,
    Segments(id): Segments<String>,
    headers: HeaderMap,
    request: Bytes,
) -> Answer {
    let args: SandboxExecArgs = body(&request)?;
    if args.cmd.is_empty() {
        return Err(invalid("cmd: an exec names a command"));
    }
    let sandbox = home.sandboxes.get(&id)?;
    let timeout = args.timeout_ms.map(Duration::from_millis);
    let job = sandbox.exec(args.cmd, args.stdin, timeout).await?;
    let answered = job.get();
    let location = format!("/sandboxes/{id}/jobs/{}", answered.id);
    let streamed = headers
        .get(header::ACCEPT)
        .and_then(|accept| accept.to_str().ok())
        .is_some_and(|accept| accept.contains("text/event-stream"));
    if !streamed {
        return Ok(created(location, &answered));
    }
    let said = ascii(&serde_json::to_string(&answered).expect("a job serializes"));
    let mut log = events(job.log(), &HeaderMap::new());
    *log.status_mut() = StatusCode::CREATED;
    let header = |value: &str| header::HeaderValue::from_str(value).map_err(internal);
    log.headers_mut()
        .insert(header::LOCATION, header(&location)?);
    log.headers_mut().insert(JOB_HEADER, header(&said)?);
    Ok(log)
}

/// `json` with every character past ASCII escaped, `\uXXXX` as JSON writes one: the same
/// JSON, and a header value.
fn ascii(json: &str) -> String {
    let mut escaped = String::with_capacity(json.len());
    for c in json.chars() {
        if c.is_ascii() {
            escaped.push(c);
        } else {
            for unit in c.encode_utf16(&mut [0; 2]) {
                escaped.push_str(&format!("\\u{unit:04x}"));
            }
        }
    }
    escaped
}

async fn sandbox_job_get(
    Extension(home): Extension<Arc<Home>>,
    Segments((id, job)): Segments<(String, String)>,
) -> Answer {
    Ok(json(&home.sandboxes.get(&id)?.job(&job)?.get()))
}

/// Kill it if it runs, and remove it.
async fn sandbox_job_remove(
    Extension(home): Extension<Arc<Home>>,
    Segments((id, job)): Segments<(String, String)>,
) -> Answer {
    home.sandboxes.get(&id)?.remove_job(&job)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn sandbox_job_signal(
    Extension(home): Extension<Arc<Home>>,
    Segments((id, job)): Segments<(String, String)>,
    request: Bytes,
) -> Answer {
    let args: SandboxSignalArgs = body(&request)?;
    let signal = signal_number(&args.signal)
        .ok_or_else(|| invalid(format!("signal: no signal is named {}", args.signal)))?;
    home.sandboxes.get(&id)?.signal(&job, signal)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// Write to the stdin of a job started with one, and close it if `eof`.
async fn sandbox_job_stdin(
    Extension(home): Extension<Arc<Home>>,
    Segments((id, job)): Segments<(String, String)>,
    request: Bytes,
) -> Answer {
    let args: SandboxStdinArgs = body(&request)?;
    let sandbox = home.sandboxes.get(&id)?;
    sandbox.write_stdin(&job, args.data.unwrap_or_default(), args.eof)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn sandbox_job_events(
    Extension(home): Extension<Arc<Home>>,
    Segments((id, job)): Segments<(String, String)>,
    headers: HeaderMap,
) -> Answer {
    Ok(events(home.sandboxes.get(&id)?.job(&job)?.log(), &headers))
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

impl From<SandboxError> for Refused {
    fn from(e: SandboxError) -> Refused {
        match e {
            SandboxError::Invalid(_) => invalid(e),
            SandboxError::NotFound(_) => not_found(e),
            SandboxError::Unsupported(_) => {
                refused(StatusCode::UNPROCESSABLE_ENTITY, "unsupported", e)
            }
            SandboxError::Exists(_) => refused(StatusCode::CONFLICT, "exists", e),
            SandboxError::NotStarted(_) => {
                refused(StatusCode::UNPROCESSABLE_ENTITY, "not_started", e)
            }
            SandboxError::Ended | SandboxError::JobEnded(_) => {
                refused(StatusCode::CONFLICT, "ended", e)
            }
            SandboxError::Stopped(_) => refused(StatusCode::CONFLICT, "stopped", e),
            SandboxError::Running(_) => refused(StatusCode::CONFLICT, "running", e),
            SandboxError::Refused {
                status,
                code,
                message,
            } => refused(
                StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                code,
                message,
            ),
            SandboxError::Failed(_) | SandboxError::Build(_) => internal(e),
            SandboxError::Image(e) => e.into(),
            SandboxError::Snapshot(e) => e.into(),
            SandboxError::Volume(e) => e.into(),
            SandboxError::Machine(e) => e.into(),
            SandboxError::Join(e) => e.into(),
        }
    }
}

impl From<tokio::task::JoinError> for Refused {
    fn from(e: tokio::task::JoinError) -> Refused {
        internal(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_escaped_to_ascii_reads_as_it_was() {
        let job = serde_json::json!({"cmd": ["echo", "héllo 世界 🎉"]});
        let escaped = ascii(&job.to_string());
        assert!(escaped.is_ascii(), "{escaped}");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&escaped).unwrap(),
            job
        );
    }

    #[test]
    fn a_name_asked_for_is_decoded() {
        assert_eq!(percent_decode("my%20box+2").unwrap(), "my box 2");
        assert_eq!(percent_decode("%E4%B8%96").unwrap(), "世");
        assert!(percent_decode("a%2").is_err());
    }
}
