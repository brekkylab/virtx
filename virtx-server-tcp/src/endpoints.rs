//! The endpoints `virtx-core`'s `PROTOCOL.md` names, but the images' registry: those in a project
//! ([`router`]), each answered from the [`Home`] of the project its request reaches, which
//! [`access`](crate::access) puts in its extensions; and those of the projects themselves
//! ([`projects`]), answered from the [`Access`] there.

use std::collections::HashMap;
use std::ffi::OsString;
use std::fmt::Display;
use std::future::Future;
use std::hash::{BuildHasher as _, Hasher as _, RandomState};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Extension, FromRequest, FromRequestParts, Request};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post, put};
use http::{HeaderMap, StatusCode, header};
use http_body_util::BodyExt as _;
use serde::{Serialize, de::DeserializeOwned};
use tokio::net::TcpStream;
use tokio::task::AbortHandle;

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use virtx_core::{
    DEFAULT_PROJECT, ImageBuildArgs, ImageJob, ImageJobEnd, ImageJobEntry, ImageJobKind,
    ImagePruneOutput, ImagePullArgs, ImagePushArgs, Log, ProjectCreateArgs, ProjectListOutputItem,
    ProjectMemberListOutputItem, ProjectMemberSetArgs, Role, SandboxCreateArgs, SandboxExecArgs,
    SandboxPort, SandboxSignalArgs, SandboxStdinArgs, VolumeCreateArgs, VolumeListOutputItem,
    VolumeMountCreateArgs, VolumeMountListOutputItem, described_type, from_body,
};
use virtx_uvm::{
    Activity, DEFAULT_MEMORY_MIB, DEFAULT_VCPUS, Digest, Error as FstreeError, Files,
    ImageConfiguration, ImageHold, ImageStore, ImageStoreError, LOG_LIMIT, MachineError,
    MachineSpec, Network, Os, PublicRegistries, Reference, RegistryClient, RegistryProxy,
    RegistryServer, Sandbox, SandboxError, UnknownSandbox, VolumeError, VolumeStore, build_step,
    park_after, signal_number,
};

use virtx_uvm::ProjectError;

use crate::access::Access;
use crate::access::Principal;
use crate::project::Project;

/// An image job, with the entries its log carries.
type ImageJobOf = Job<ImageJob, ImageJobEntry>;

/// How old what nothing needs has to be before a prune takes it: a pull, push or build in
/// flight has put it there and not yet pointed a reference at it.
pub const PRUNE_GRACE: Duration = Duration::from_secs(60 * 60);

/// How long an image job that has ended is kept, for its end to be read, before it is removed.
const IMAGE_JOB_KEPT: Duration = Duration::from_secs(60);

/// What one project's requests reach: the stores, and the sandboxes and jobs its members made
/// from them, which are theirs together.
pub struct Home {
    images: ImageStore,
    registry: Arc<RegistryServer>,
    /// What its image jobs -- a pull, a push, a build's base -- connect to registries through.
    registries: Arc<dyn RegistryProxy>,
    /// Docker Hub's mirror, which its anonymous pulls of Docker Hub's images go to first.
    mirror: Option<Arc<str>>,
    /// The home, under whose `sandbox/` the sandboxes keep their upper disks.
    pub root: PathBuf,
    volumes: VolumeStore,
    /// The files of the volumes and sandboxes, over WebDAV.
    files: Files,
    /// This binary, as each machine is spawned.
    vm: Vec<OsString>,
    /// The sandboxes, by id, until each is removed.
    sandboxes: Mutex<HashMap<String, Held>>,
    /// Each sandbox kept here that could not be known again, by id, until it is removed: listed
    /// with why, and refused all but its `DELETE`.
    unknown: Mutex<HashMap<String, UnknownSandbox>>,
    /// The image jobs, by id, and what cancels each, until each is removed or a while after
    /// it ends.
    image_jobs: Mutex<HashMap<String, (Arc<ImageJobOf>, AbortHandle)>>,
    /// Set once its project is to be removed: no sandbox or image job is added after.
    closed: AtomicBool,
}

impl Home {
    /// The images in `images`, which `registry` answers for, and the volumes and the
    /// sandboxes' upper disks under `root`, with each sandbox a server before it kept there
    /// known again, parked ([`Sandbox::kept`]). Its image jobs connect to registries on public
    /// addresses alone, through [`PublicRegistries`] with none named, unless the [`Access`] it
    /// is made for says otherwise ([`Access::through`]). Blocking work, as knowing each
    /// sandbox again is.
    pub fn new(
        images: ImageStore,
        registry: Arc<RegistryServer>,
        root: &Path,
    ) -> anyhow::Result<Home> {
        let volumes = VolumeStore::new(root)?;
        let vm: Vec<OsString> = vec![
            std::env::current_exe()?.into(),
            crate::serve::MACHINE_SUBCOMMAND.into(),
        ];
        let kept = Sandbox::kept(&images, root, &volumes, vm.clone(), park_after());
        let unknown = kept
            .unknown
            .into_iter()
            .map(|unknown| (unknown.id().to_owned(), unknown))
            .collect();
        let sandboxes = kept
            .known
            .into_iter()
            .map(|sandbox| {
                let capabilities = sandbox.keys();
                let id = sandbox.id().to_owned();
                (
                    id,
                    Held {
                        sandbox,
                        capabilities,
                    },
                )
            })
            .collect();
        Ok(Home {
            images,
            registry,
            registries: Arc::new(PublicRegistries::default()),
            mirror: None,
            files: Files::new(volumes.clone()),
            root: root.to_owned(),
            volumes,
            vm,
            sandboxes: Mutex::new(sandboxes),
            unknown: Mutex::new(unknown),
            image_jobs: Mutex::default(),
            closed: AtomicBool::default(),
        })
    }

    /// This, its image jobs connecting to registries through `registries`, and its anonymous
    /// pulls of Docker Hub's images going to `mirror` first.
    pub(crate) fn through(
        mut self,
        registries: Arc<dyn RegistryProxy>,
        mirror: Option<Arc<str>>,
    ) -> Home {
        self.registries = registries;
        self.mirror = mirror;
        self
    }

    pub fn images(&self) -> &ImageStore {
        &self.images
    }

    /// What answers this home's `/images/oci/v2/`.
    pub fn registry(&self) -> &Arc<RegistryServer> {
        &self.registry
    }

    /// A tunnel to the port `capability` opens, in whichever of this home's sandboxes it is
    /// one of.
    pub fn tunnel(&self, capability: &str) -> Option<Tunnel> {
        let sandboxes = self.sandboxes.lock().unwrap();
        sandboxes.values().find_map(|held| {
            let port = *held.capabilities.get(capability)?;
            Some(Tunnel {
                activity: held.sandbox.activity(),
                port,
            })
        })
    }

    /// Close it to every sandbox and image job from now on, for its project to be removed:
    /// `false`, and left open, while a sandbox of this process or another lives here. Decided
    /// under the sandboxes' lock, which a sandbox is added under once it holds its upper disk:
    /// one made meanwhile is either seen here or sees it closed.
    pub fn close(&self) -> bool {
        let _sandboxes = self.sandboxes.lock().unwrap();
        if Sandbox::lives_under(&self.root) {
            return false;
        }
        self.closed.store(true, Ordering::SeqCst);
        true
    }

    /// Cancel every image job here, and close every sandbox, as removing each does, side by
    /// side: blocks until each machine has stopped and synced what it wrote.
    pub fn close_all(&self) {
        for (_, (job, task)) in self.image_jobs.lock().unwrap().drain() {
            cancel(&job, &task);
        }
        let closed: Vec<_> = self
            .sandboxes
            .lock()
            .unwrap()
            .drain()
            .map(|(_, held)| held.sandbox)
            .collect();
        std::thread::scope(|scope| {
            for sandbox in &closed {
                scope.spawn(|| sandbox.close());
            }
        });
    }

    /// Cancel every image job here, and let go of every sandbox as this server ends, side by
    /// side, each kept for the next server over this home ([`Sandbox::leave`]): blocks until
    /// each machine has stopped and synced what it wrote.
    pub fn leave_all(&self) {
        for (_, (job, task)) in self.image_jobs.lock().unwrap().drain() {
            cancel(&job, &task);
        }
        let left: Vec<_> = self
            .sandboxes
            .lock()
            .unwrap()
            .drain()
            .map(|(_, held)| held.sandbox)
            .collect();
        std::thread::scope(|scope| {
            for sandbox in &left {
                scope.spawn(|| sandbox.leave());
            }
        });
    }

    /// The sandbox `id`: refused `409`, `ended`, for one not known again, which takes only its
    /// `DELETE`.
    fn sandbox(&self, id: &str) -> Result<Arc<Sandbox>, Refused> {
        let sandboxes = self.sandboxes.lock().unwrap();
        if let Some(held) = sandboxes.get(id) {
            return Ok(Arc::clone(&held.sandbox));
        }
        if let Some(unknown) = self.unknown.lock().unwrap().get(id) {
            return Err(refused(
                StatusCode::CONFLICT,
                "ended",
                format!(
                    "sandbox {id} was not known again since its server restarted, and takes only \
                     its DELETE: {}",
                    unknown.why()
                ),
            ));
        }
        Err(not_found(format!("no sandbox here has the id {id}")))
    }
}

/// A connection through one of a sandbox's ports, counted until it drops: a sandbox with one
/// is in use, whatever its jobs do.
pub struct Tunnel {
    activity: Activity,
    port: u16,
}

impl Tunnel {
    /// The port, in the sandbox's machine while it runs.
    pub async fn connect(&self) -> io::Result<TcpStream> {
        self.activity.sandbox().connect(self.port).await
    }
}

/// A sandbox, and the port each capability its making answered opens.
struct Held {
    sandbox: Arc<Sandbox>,
    capabilities: HashMap<String, u16>,
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
        .route("/volumes/{name}/files", any(volume_files))
        .route("/volumes/{name}/files/", any(volume_files))
        .route("/volumes/{name}/files/{*path}", any(volume_files))
        .route("/sandboxes/{id}/files", any(sandbox_files))
        .route("/sandboxes/{id}/files/", any(sandbox_files))
        .route("/sandboxes/{id}/files/{*path}", any(sandbox_files))
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

/// The projects, and their members, each request answered as its sender, from the [`Access`] in
/// its extensions.
pub fn projects() -> Router {
    Router::new()
        .route("/projects/", get(project_list).post(project_create))
        .route(
            "/projects/{project}",
            get(project_get).delete(project_remove),
        )
        .route("/projects/{project}/members/", get(project_member_list))
        .route(
            "/projects/{project}/members/{user}",
            put(project_member_set).delete(project_member_remove),
        )
}

type Answer = Result<Response, Refused>;

/// What a path segment keeps as it is: the unreserved characters of RFC 3986.
const SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// Who sends a request with `headers`, the project `named` names to them, and their role in
/// it: refused as [`Access::principal`] refuses, and `403` where they have none, or it does not
/// exist.
async fn role_in(
    access: &Access,
    headers: &HeaderMap,
    named: &str,
) -> Result<(Principal, Project, Role), Refused> {
    let who = access.principal(headers).await.map_err(refusal)?;
    let reached = match access.project(&who, Some(named)).await? {
        Some((id, role)) => access.membership().project(&id).await?.map(|p| (p, role)),
        None => None,
    };
    match reached {
        Some((project, role)) => Ok((who, project, role)),
        None => Err(forbidden(format!("project {named} is not one you reach"))),
    }
}

/// `project` as the server answers it to one in `role` in it.
fn answer(project: Project, role: Role) -> ProjectListOutputItem {
    ProjectListOutputItem {
        id: project.id.to_string(),
        owner: project.owner.user().map(str::to_owned),
        role,
        name: project.name,
    }
}

/// Every project the sender has a role in, their own among them, made if it is not there.
async fn project_list(Extension(access): Extension<Arc<Access>>, headers: HeaderMap) -> Answer {
    let who = access.principal(&headers).await.map_err(refusal)?;
    access.project(&who, None).await?;
    let listed: Vec<_> = access
        .membership()
        .projects(&who)
        .await?
        .into_iter()
        .map(|(project, role)| answer(project, role))
        .collect();
    Ok(json(&listed))
}

/// Make a project of the sender's, them its owner: refused for [`DEFAULT_PROJECT`], which is
/// each user's own.
async fn project_create(
    Extension(access): Extension<Arc<Access>>,
    headers: HeaderMap,
    request: Bytes,
) -> Answer {
    let who = access.principal(&headers).await.map_err(refusal)?;
    let args: ProjectCreateArgs = body(&request)?;
    if args.name == DEFAULT_PROJECT {
        return Err(invalid(format!(
            "{DEFAULT_PROJECT:?} is each user's own project, made for them"
        )));
    }
    let made = access.membership().create(&who, &args.name).await?;
    let made = answer(made, Role::Owner);
    Ok(created(format!("/projects/{}", made.id), &made))
}

async fn project_get(
    Extension(access): Extension<Arc<Access>>,
    Segments(named): Segments<String>,
    headers: HeaderMap,
) -> Answer {
    let (_, project, role) = role_in(&access, &headers, &named).await?;
    Ok(json(&answer(project, role)))
}

/// Remove a project and everything in it, its image jobs cancelled: an owner's to do, and
/// refused while a sandbox lives in it.
async fn project_remove(
    Extension(access): Extension<Arc<Access>>,
    Segments(named): Segments<String>,
    headers: HeaderMap,
) -> Answer {
    let (_, project, role) = role_in(&access, &headers, &named).await?;
    if role != Role::Owner {
        return Err(forbidden(format!("only an owner removes project {named}")));
    }
    let id = project.id;
    if !access.close(&id) {
        return Err(refused(
            StatusCode::CONFLICT,
            "in_use",
            format!("a sandbox lives in project {named}"),
        ));
    }
    let removed = async {
        access.membership().remove(&id).await?;
        let store = access.store().clone();
        tokio::task::spawn_blocking(move || store.remove(&id))
            .await
            .map_err(internal)??;
        Ok::<_, Refused>(())
    }
    .await;
    // Once removed, its id stays taken: no id is given again, and a request that read the
    // record before it was forgotten makes no home for it.
    if removed.is_err() {
        access.release(&id);
    }
    removed?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn project_member_list(
    Extension(access): Extension<Arc<Access>>,
    Segments(named): Segments<String>,
    headers: HeaderMap,
) -> Answer {
    let (_, project, _) = role_in(&access, &headers, &named).await?;
    let members: Vec<_> = project
        .members
        .into_iter()
        .map(|(user, role)| ProjectMemberListOutputItem { user, role })
        .collect();
    Ok(json(&members))
}

/// Give `user` a role in a project, adding them if they are no member yet: an owner's to do.
async fn project_member_set(
    Extension(access): Extension<Arc<Access>>,
    Segments((named, user)): Segments<(String, String)>,
    headers: HeaderMap,
    request: Bytes,
) -> Answer {
    let (_, project, role) = role_in(&access, &headers, &named).await?;
    if role != Role::Owner {
        return Err(forbidden(format!(
            "only an owner changes the members of project {named}"
        )));
    }
    let args: ProjectMemberSetArgs = body(&request)?;
    let had = access
        .membership()
        .set_member(&project.id, &user, Some(args.role))
        .await?;
    let set = ProjectMemberListOutputItem {
        user,
        role: args.role,
    };
    if had.is_some() {
        return Ok(json(&set));
    }
    let user = utf8_percent_encode(&set.user, SEGMENT);
    Ok(created(
        format!("/projects/{}/members/{user}", project.id),
        &set,
    ))
}

/// Take `user`'s role in a project away: an owner's to do, or the user's own, leaving it.
async fn project_member_remove(
    Extension(access): Extension<Arc<Access>>,
    Segments((named, user)): Segments<(String, String)>,
    headers: HeaderMap,
) -> Answer {
    let (sender, project, role) = role_in(&access, &headers, &named).await?;
    if role != Role::Owner && sender.user != user {
        return Err(forbidden(format!(
            "only an owner removes another member of project {named}"
        )));
    }
    let had = access
        .membership()
        .set_member(&project.id, &user, None)
        .await?;
    if had.is_none() {
        return Err(not_found(format!("{user} is no member of project {named}")));
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn image_job_list(Extension(home): Extension<Arc<Home>>) -> Answer {
    let jobs = home.image_jobs.lock().unwrap();
    let listed: Vec<_> = jobs
        .values()
        .map(|(job, _)| job.job.lock().unwrap().clone())
        .collect();
    Ok(json(&listed))
}

/// Start a job of `kind` for `reference`: `work`, given what logs a line of it, run until it
/// ends or is cancelled, and kept [`IMAGE_JOB_KEPT`] after.
fn image_job<Fut>(
    home: &Arc<Home>,
    kind: ImageJobKind,
    reference: String,
    work: impl FnOnce(Box<dyn FnMut(String) + Send>) -> Fut,
) -> Answer
where
    Fut: Future<Output = Result<Digest, Failure>> + Send + 'static,
{
    let mut jobs = home.image_jobs.lock().unwrap();
    // Under the jobs' lock, which closing the home cancels them under after.
    if home.closed.load(Ordering::SeqCst) {
        return Err(forbidden("this project is being removed"));
    }
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
    let (ended, kept, removed) = (Arc::clone(&job), Arc::clone(home), id.clone());
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
        tokio::time::sleep(IMAGE_JOB_KEPT).await;
        kept.image_jobs.lock().unwrap().remove(&removed);
        ended.remove();
    });
    jobs.insert(id.clone(), (Arc::clone(&job), task.abort_handle()));
    Ok(created(
        format!("/images/jobs/{id}"),
        &*job.job.lock().unwrap(),
    ))
}

async fn image_pull(Extension(home): Extension<Arc<Home>>, Whole(request): Whole) -> Answer {
    let args: ImagePullArgs = body(&request)?;
    let reference = reference(&args.reference)?;
    let (images, registries) = (home.images.clone(), Arc::clone(&home.registries));
    let mirror = home.mirror.clone();
    image_job(
        &home,
        ImageJobKind::Pull,
        args.reference,
        move |report| async move {
            let registry = RegistryClient::new(images.clone(), args.auth)
                .with_mirror(mirror.as_deref())
                .through(Some(&*registries))?;
            let (_, image) = registry.pull(&reference, report).await?;
            let (store, tagged) = (images.clone(), image.clone());
            tokio::task::spawn_blocking(move || store.insert_tag(&reference, &tagged)).await??;
            Ok(image)
        },
    )
}

async fn image_push(Extension(home): Extension<Arc<Home>>, Whole(request): Whole) -> Answer {
    let args: ImagePushArgs = body(&request)?;
    let reference = reference(&args.reference)?;
    let (images, registries) = (home.images.clone(), Arc::clone(&home.registries));
    image_job(
        &home,
        ImageJobKind::Push,
        args.reference,
        move |report| async move {
            let registry = RegistryClient::new(images, args.auth).through(Some(&*registries))?;
            Ok(registry.push(&reference, report).await?)
        },
    )
}

async fn image_build(Extension(home): Extension<Arc<Home>>, Whole(request): Whole) -> Answer {
    let args: ImageBuildArgs = body(&request)?;
    let reference = reference(&args.reference)?;
    if args.recipe.base != "scratch" {
        self::reference(&args.recipe.base)?;
    }
    let (images, vm) = (home.images.clone(), home.vm.clone());
    let (registries, mirror) = (Arc::clone(&home.registries), home.mirror.clone());
    let recipe = args.recipe;
    image_job(
        &home,
        ImageJobKind::Build,
        args.reference,
        move |mut report| async move {
            let (mut image, mut layers, _base) = base(
                &images,
                &*registries,
                mirror.as_deref(),
                &recipe.base,
                &mut report,
            )
            .await?;
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
            let digest = tokio::task::spawn_blocking(move || {
                let digest = images.insert_image(&image, &layers)?;
                images.insert_tag(&reference, &digest)?;
                Ok::<_, ImageStoreError>(digest)
            })
            .await??;
            Ok(digest)
        },
    )
}

/// A build's base: its config and layers, and its image held for as long as the build runs.
/// Nothing for `scratch`, as in a Dockerfile; else the image tagged `base` here, or the one
/// pulled into the store now, anonymously, through `registries` and from `mirror` first if it
/// is Docker Hub's, and left untagged: a build tags only what it builds.
async fn base(
    images: &ImageStore,
    registries: &dyn RegistryProxy,
    mirror: Option<&str>,
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
                .with_mirror(mirror)
                .through(Some(registries))?
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
    cancel(&job, &task);
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// Stop `job`'s `task`, leaving no tag changed, end it as cancelled, and remove its log.
fn cancel(job: &ImageJobOf, task: &AbortHandle) {
    task.abort();
    job.end(
        |job| (job.running, job.error) = (false, Some("cancelled".into())),
        ImageJobEntry::End(ImageJobEnd {
            digest: None,
            error: Some("cancelled".into()),
        }),
    );
    job.remove();
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

async fn volume_create(Extension(home): Extension<Arc<Home>>, Whole(request): Whole) -> Answer {
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
    Whole(request): Whole,
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
async fn volume_files(
    Extension(home): Extension<Arc<Home>>,
    Segments(segments): Segments<HashMap<String, String>>,
    request: axum::extract::Request,
) -> Response {
    home.files.volume(&segments["name"], request).await
}

/// A request to the files of a sandbox, over WebDAV, asked of its machine, booted again if it
/// was parked: one whose machine has exited answers `409`.
async fn sandbox_files(
    Extension(home): Extension<Arc<Home>>,
    Segments(segments): Segments<HashMap<String, String>>,
    request: axum::extract::Request,
) -> Answer {
    let id = &segments["id"];
    let sandbox = home.sandbox(id)?;
    // In use from here until the answer has been sent, booting it again if it was parked.
    let activity = sandbox.activity();
    let machine = sandbox.machine().await?;
    Ok(home.files.sandbox(id, machine, activity, request).await)
}

async fn sandbox_list(Extension(home): Extension<Arc<Home>>) -> Answer {
    let sandboxes = home.sandboxes.lock().unwrap();
    let mut listed: Vec<_> = sandboxes
        .values()
        .map(|held| held.sandbox.answer())
        .collect();
    let unknown = home.unknown.lock().unwrap();
    listed.extend(unknown.values().map(UnknownSandbox::answer));
    Ok(json(&listed))
}

async fn sandbox_create(Extension(home): Extension<Arc<Home>>, Whole(request): Whole) -> Answer {
    let args: SandboxCreateArgs = body(&request)?;

    // Each port behind a capability of its own, which only this answer carries: who has the
    // URL reaches the port, and nothing else. Relative, to the URL this request came to, since
    // what reaches this server from outside is not this server's to know. Kept with the
    // sandbox, so that its URLs reach it across this server's restart.
    let mut opened = Vec::new();
    let mut capabilities = HashMap::new();
    for &port in &args.ports {
        let capability = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        opened.push(SandboxPort {
            port,
            url: format!("ports/{capability}"),
        });
        capabilities.insert(capability, port);
    }
    let sandbox = Sandbox::create(
        &home.images,
        &home.root,
        &home.volumes,
        &args,
        home.vm.clone(),
        park_after(),
        Some(capabilities.clone()),
    )
    .await?;

    let id = sandbox.id().to_owned();
    let held = Held {
        sandbox: Arc::clone(&sandbox),
        capabilities,
    };
    let closed = {
        let mut sandboxes = home.sandboxes.lock().unwrap();
        let closed = home.closed.load(Ordering::SeqCst);
        if !closed {
            sandboxes.insert(id.clone(), held);
        }
        closed
    };
    if closed {
        tokio::task::spawn_blocking(move || sandbox.close()).await?;
        return Err(forbidden("this project is being removed"));
    }
    let answer = virtx_core::Sandbox {
        ports: opened,
        ..sandbox.answer()
    };
    Ok(created(format!("/sandboxes/{id}"), &answer))
}

async fn sandbox_get(
    Extension(home): Extension<Arc<Home>>,
    Segments(id): Segments<String>,
) -> Answer {
    if let Some(unknown) = home.unknown.lock().unwrap().get(&id) {
        return Ok(json(&unknown.answer()));
    }
    Ok(json(&home.sandbox(&id)?.answer()))
}

async fn sandbox_remove(
    Extension(home): Extension<Arc<Home>>,
    Segments(id): Segments<String>,
) -> Answer {
    let unknown = home.unknown.lock().unwrap().remove(&id);
    if let Some(unknown) = unknown {
        tokio::task::spawn_blocking(move || unknown.remove()).await?;
        return Ok(StatusCode::NO_CONTENT.into_response());
    }
    let removed = home.sandboxes.lock().unwrap().remove(&id);
    let removed = removed.ok_or_else(|| not_found(format!("no sandbox here has the id {id}")))?;
    tokio::task::spawn_blocking(move || removed.sandbox.close()).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn sandbox_job_list(
    Extension(home): Extension<Arc<Home>>,
    Segments(id): Segments<String>,
) -> Answer {
    Ok(json(&home.sandbox(&id)?.jobs()))
}

async fn sandbox_exec(
    Extension(home): Extension<Arc<Home>>,
    Segments(id): Segments<String>,
    Whole(request): Whole,
) -> Answer {
    let args: SandboxExecArgs = body(&request)?;
    if args.cmd.is_empty() {
        return Err(invalid("cmd: an exec names a command"));
    }
    let sandbox = home.sandbox(&id)?;
    let timeout = args.timeout_ms.map(Duration::from_millis);
    let job = sandbox.exec(args.cmd, args.stdin, timeout).await?.get();
    Ok(created(format!("/sandboxes/{id}/jobs/{}", job.id), &job))
}

async fn sandbox_job_get(
    Extension(home): Extension<Arc<Home>>,
    Segments((id, job)): Segments<(String, String)>,
) -> Answer {
    Ok(json(&home.sandbox(&id)?.job(&job)?.get()))
}

/// Kill it if it runs, and remove it.
async fn sandbox_job_remove(
    Extension(home): Extension<Arc<Home>>,
    Segments((id, job)): Segments<(String, String)>,
) -> Answer {
    home.sandbox(&id)?.remove_job(&job)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn sandbox_job_signal(
    Extension(home): Extension<Arc<Home>>,
    Segments((id, job)): Segments<(String, String)>,
    Whole(request): Whole,
) -> Answer {
    let args: SandboxSignalArgs = body(&request)?;
    let signal = signal_number(&args.signal)
        .ok_or_else(|| invalid(format!("signal: no signal is named {}", args.signal)))?;
    home.sandbox(&id)?.signal(&job, signal)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// Write to the stdin of a job started with one, and close it if `eof`: answered once it is in
/// the process's stdin, which takes a pipe's worth before the process reads any. Refused `409`,
/// `ended`, for one started without, one whose stdin is closed, one that has ended, and a write
/// its process closed its stdin under, which says how much of it went in.
async fn sandbox_job_stdin(
    Extension(home): Extension<Arc<Home>>,
    Segments((id, job)): Segments<(String, String)>,
    Whole(request): Whole,
) -> Answer {
    let args: SandboxStdinArgs = body(&request)?;
    let sandbox = home.sandbox(&id)?;
    sandbox
        .write_stdin(&job, args.data.unwrap_or_default(), args.eof)
        .await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn sandbox_job_events(
    Extension(home): Extension<Arc<Home>>,
    Segments((id, job)): Segments<(String, String)>,
    headers: HeaderMap,
) -> Answer {
    Ok(events(home.sandbox(&id)?.job(&job)?.log(), &headers))
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

/// The most a request's body may be, but a file's, which is streamed: `413` `too_large` past it.
const BODY_LIMIT: usize = 2 << 20;

/// A request's body, whole: refused `413` `too_large` past [`BODY_LIMIT`], and `400` `invalid`
/// if it broke off, as any refusal here is, in PROTOCOL.md's envelope rather than as text.
///
/// A body past the limit is still read to its end, and what comes after the limit let go of as
/// it comes, before it is refused: answered with its body unread, the connection is closed with
/// it unread and reset, on any system, so a client still sending sees the reset and never the
/// `413`. The files endpoints read the rest of a body for the same reason. What is read has no
/// bound: a client that sends a JSON body here has been let in already, and may write a file of
/// any size.
struct Whole(Bytes);

impl<S: Send + Sync> FromRequest<S> for Whole {
    type Rejection = Refused;

    async fn from_request(request: Request, _: &S) -> Result<Self, Refused> {
        let mut body = request.into_body();
        let mut whole = Vec::new();
        let mut over = false;
        while let Some(frame) = body.frame().await {
            let frame = frame.map_err(|e| invalid(format!("the body broke off: {e}")))?;
            let Ok(data) = frame.into_data() else {
                continue;
            };
            if over {
                continue;
            }
            if whole.len() + data.len() > BODY_LIMIT {
                over = true;
                whole = Vec::new();
                continue;
            }
            whole.extend_from_slice(&data);
        }
        if over {
            return Err(refused(
                StatusCode::PAYLOAD_TOO_LARGE,
                "too_large",
                format!(
                    "the body is over the server's limit of {} MiB",
                    BODY_LIMIT >> 20
                ),
            ));
        }
        Ok(Whole(Bytes::from(whole)))
    }
}

/// A path's segments read as a `T`, as axum's `Path` reads them: refused `400` `invalid` when
/// they do not read as one, in PROTOCOL.md's envelope and the server's words.
struct Segments<T>(T);

impl<T, S> FromRequestParts<S> for Segments<T>
where
    T: DeserializeOwned + Send,
    S: Send + Sync,
{
    type Rejection = Refused;

    async fn from_request_parts(
        parts: &mut http::request::Parts,
        state: &S,
    ) -> Result<Self, Refused> {
        match axum::extract::Path::<T>::from_request_parts(parts, state).await {
            Ok(axum::extract::Path(segments)) => Ok(Segments(segments)),
            Err(e) if e.status().is_server_error() => Err(internal(e.body_text())),
            Err(e) => Err(invalid(unread_path(&e))),
        }
    }
}

/// Why a path's segments did not read, naming the segment and what it takes.
fn unread_path(rejection: &axum::extract::rejection::PathRejection) -> String {
    use axum::extract::path::ErrorKind;
    use axum::extract::rejection::PathRejection;
    let PathRejection::FailedToDeserializePathParams(failed) = rejection else {
        return "the path does not read".to_owned();
    };
    match failed.kind() {
        ErrorKind::ParseErrorAtKey {
            key,
            value,
            expected_type,
        } => format!("{key}: {}, not {value:?}", described_type(expected_type)),
        ErrorKind::ParseErrorAtIndex {
            value,
            expected_type,
            ..
        }
        | ErrorKind::ParseError {
            value,
            expected_type,
        } => format!("the path: {}, not {value:?}", described_type(expected_type)),
        ErrorKind::InvalidUtf8InPathParam { key } => {
            format!("{key}: UTF-8 once its escapes are read, and it is not")
        }
        _ => "the path does not read".to_owned(),
    }
}

/// `request` read as a `T`; refused naming the member that does not read and what it takes.
fn body<T: DeserializeOwned>(request: &Bytes) -> Result<T, Refused> {
    from_body(request).map_err(invalid)
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
/// A refusal: its status, `code` and `message`, and the members its error has beside them.
pub struct Refused(
    StatusCode,
    &'static str,
    String,
    serde_json::Map<String, serde_json::Value>,
);

pub fn refused(status: StatusCode, code: &'static str, message: impl Display) -> Refused {
    Refused(status, code, message.to_string(), serde_json::Map::new())
}

impl Refused {
    /// It with `name` beside its `code` and `message`.
    fn with(mut self, name: &str, value: impl Into<serde_json::Value>) -> Refused {
        self.3.insert(name.to_owned(), value.into());
        self
    }
}

fn invalid(message: impl Display) -> Refused {
    refused(StatusCode::BAD_REQUEST, "invalid", message)
}

fn forbidden(message: impl Display) -> Refused {
    refused(StatusCode::FORBIDDEN, "forbidden", message)
}

/// A request [`Access`] refused with `status`, as the endpoints refuse one.
pub fn refusal(status: StatusCode) -> Refused {
    match status {
        StatusCode::UNAUTHORIZED => refused(status, "unauthorized", "no token this server knows"),
        StatusCode::FORBIDDEN => forbidden("not a member of the project, or not from here"),
        StatusCode::BAD_REQUEST => invalid("the virtx-project header is not a project's name"),
        _ => internal("reaching the project failed"),
    }
}

fn not_found(message: impl Display) -> Refused {
    refused(StatusCode::NOT_FOUND, "not_found", message)
}

fn internal(message: impl Display) -> Refused {
    refused(StatusCode::INTERNAL_SERVER_ERROR, "internal", message)
}

impl IntoResponse for Refused {
    fn into_response(self) -> Response {
        let Refused(status, code, message, members) = self;
        let mut error = serde_json::json!({"code": code, "message": message});
        error.as_object_mut().unwrap().extend(members);
        let body = serde_json::json!({ "error": error });
        (status, json(&body)).into_response()
    }
}

/// What a refusal says of an operation on the host's files that failed with `e`: `e`'s kind.
/// `message` can name the server's own paths, so it goes to the server's stderr instead, and the
/// refusal is answered even when stderr cannot take it, as on a disk that is full.
fn io_failure(e: &std::io::Error, message: impl Display) -> String {
    let _ = writeln!(io::stderr(), "{message}");
    format!("the server's files: {}", e.kind())
}

/// The refusal an operation on the host's files that failed with `e` is.
fn io_refused(e: &std::io::Error, message: impl Display) -> Refused {
    let said = io_failure(e, message);
    match e.kind() {
        std::io::ErrorKind::NotFound => not_found(said),
        std::io::ErrorKind::AlreadyExists => refused(StatusCode::CONFLICT, "exists", said),
        _ => internal(said),
    }
}

impl From<ProjectError> for Refused {
    fn from(e: ProjectError) -> Refused {
        match &e {
            ProjectError::InvalidName(_) => invalid(e),
            ProjectError::Unknown(_) => forbidden(e),
            ProjectError::AlreadyExists(_) => refused(StatusCode::CONFLICT, "exists", e),
            ProjectError::NoOwner(_) => refused(StatusCode::CONFLICT, "in_use", e),
            ProjectError::Io { source, .. } => internal(io_failure(source, &e)),
            // A server's own, such as its database's, which can name what it runs on.
            ProjectError::Other(_) => {
                let _ = writeln!(io::stderr(), "{e:#}");
                internal("the server's projects failed")
            }
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
            VolumeError::AlreadyExists(_) | VolumeError::MountExists(_) => {
                refused(StatusCode::CONFLICT, "exists", e)
            }
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
            ImageStoreError::Fstree(FstreeError::Io { source, .. }) => {
                internal(io_failure(source, &e))
            }
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
            SandboxError::NotStarted(_) => {
                refused(StatusCode::UNPROCESSABLE_ENTITY, "not_started", e)
            }
            SandboxError::Ended | SandboxError::JobEnded(_) => {
                refused(StatusCode::CONFLICT, "ended", e)
            }
            SandboxError::StdinCut { bytes, .. } => {
                refused(StatusCode::CONFLICT, "ended", e).with("written", bytes)
            }
            SandboxError::Image(e) => e.into(),
            SandboxError::Io { ref source, .. } => internal(io_failure(source, &e)),
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

    #[tokio::test]
    async fn a_failure_on_the_server_s_files_is_answered_without_their_paths() {
        const CONTEXT: &str = "making /home/virtx/project/a/volume/v";
        let kinds = [
            (io::ErrorKind::NotFound, StatusCode::NOT_FOUND, "not_found"),
            (io::ErrorKind::AlreadyExists, StatusCode::CONFLICT, "exists"),
            (
                io::ErrorKind::PermissionDenied,
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
            ),
        ];
        for (kind, status, code) in kinds {
            let source = || io::Error::new(kind, "under /home/virtx/project/a");
            let tree = || FstreeError::Io {
                context: CONTEXT.into(),
                source: source(),
            };
            let internal: [Refused; 3] = [
                ImageStoreError::Fstree(tree()).into(),
                ProjectError::Io {
                    context: CONTEXT.into(),
                    source: source(),
                }
                .into(),
                SandboxError::Io {
                    context: CONTEXT.into(),
                    source: source(),
                }
                .into(),
            ];
            let by_kind: [Refused; 2] = [
                VolumeError::Io {
                    context: CONTEXT.into(),
                    source: source(),
                }
                .into(),
                ImageStoreError::Io {
                    context: CONTEXT.into(),
                    source: source(),
                }
                .into(),
            ];
            let answers = by_kind.into_iter().map(|r| (r, status, code)).chain(
                internal
                    .into_iter()
                    .map(|r| (r, StatusCode::INTERNAL_SERVER_ERROR, "internal")),
            );
            for (refusal, status, code) in answers {
                let response = refusal.into_response();
                assert_eq!(response.status(), status);
                let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                    .await
                    .unwrap();
                let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(body["error"]["code"], code);
                let message = body["error"]["message"].as_str().unwrap();
                assert_eq!(message, format!("the server's files: {kind}"));
            }
        }
        let other: Refused = ProjectError::Other(anyhow::anyhow!("postgres://db.internal")).into();
        let body = axum::body::to_bytes(other.into_response().into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["error"]["message"], "the server's projects failed");
    }

    /// A write its process closed its stdin under part-way is `409`, `ended`, with how much of
    /// it went in as `written`; no other refusal carries it.
    #[tokio::test]
    async fn a_write_cut_part_way_says_how_much_went_in() {
        let answered = async |refusal: Refused| {
            let response = refusal.into_response();
            assert_eq!(response.status(), StatusCode::CONFLICT);
            let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            serde_json::from_slice::<serde_json::Value>(&body).unwrap()
        };
        let cut = answered(SandboxError::StdinCut { bytes: 5, of: 9 }.into()).await;
        assert_eq!(
            cut,
            serde_json::json!({"error": {
                "code": "ended",
                "message": "the job's stdin closed after 5 of the 9 bytes written to it",
                "written": 5,
            }})
        );
        let ended = answered(SandboxError::JobEnded("the job has ended").into()).await;
        assert_eq!(
            ended,
            serde_json::json!({"error": {"code": "ended", "message": "the job has ended"}})
        );
    }
}
