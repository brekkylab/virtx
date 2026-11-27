//! The endpoints the protocol names, but the images' registry: those in a project ([`router`]),
//! each answered from the [`Home`] of the project its request reaches, which
//! [`access`](crate::access) puts in its extensions; and those of the projects themselves
//! ([`projects`]), answered from the [`Access`] there.
//!
//! A home keeps its project's data in its own stores, and beyond this machine too where it is
//! given a [`StorageBackend`] ([`Home::backed_by`]), whose cache its stores are then.

use std::collections::{HashMap, HashSet};
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
use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade, close_code};
use axum::extract::{Extension, FromRequest, FromRequestParts, RawQuery, Request};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post, put};
use http::{HeaderMap, StatusCode, header};
use http_body_util::BodyExt as _;
use serde::{Serialize, de::DeserializeOwned};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;
use tokio::task::AbortHandle;

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use virtx_core::{
    DEFAULT_PROJECT, ImageBuildArgs, ImageJob, ImageJobEnd, ImageJobEntry, ImageJobKind,
    ImagePruneOutput, ImagePullArgs, ImagePushArgs, Log, PortSide, ProjectCreateArgs,
    ProjectListOutputItem, ProjectMemberListOutputItem, ProjectMemberSetArgs, Role,
    SandboxCreateArgs, SandboxExecArgs, SandboxExecItem, SandboxResizeArgs, SandboxSignalArgs,
    SandboxStdinArgs, Step, VolumeCreateArgs, VolumeListOutputItem, VolumeMountCreateArgs,
    VolumeMountListOutputItem, accepts_event_stream, described_type, from_body,
};
use virtx_uvm::backend::{Backing, Refusal, Stored, TagError};
use virtx_uvm::{
    BuildError, ClientPortTaker, DEFAULT_MEMORY_MIB, DEFAULT_VCPUS, Digest, Error as FstreeError,
    Files, HostPaths, ImageConfiguration, ImageHold, ImageStore, ImageStoreError, LOG_LIMIT,
    MachineError, MachineSpec, NoRoom, Os, PublicImages, PublicRegistries, Reference,
    RegistryClient, RegistryError, RegistryProxy, RegistryServer, Sandbox, SandboxError,
    StorageBackend, UnknownSandbox, VolumeError, VolumeStore, based_on, build_step, park_after,
    signal_number,
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
    /// Where its anonymous pulls are shared with other homes', if anywhere ([`Home::sharing`]).
    public: Option<Arc<dyn PublicImages>>,
    /// The home, under whose `sandbox/` the sandboxes keep their upper disks.
    pub root: PathBuf,
    volumes: VolumeStore,
    /// The files of the volumes and sandboxes, over WebDAV.
    files: Files,
    /// This binary, as each machine is spawned.
    vm: Vec<OsString>,
    /// The sandboxes, by id, until each is removed.
    sandboxes: Mutex<HashMap<String, Arc<Sandbox>>>,
    /// The names sandboxes being made have taken, each until it is made or is not.
    naming: Mutex<HashSet<String>>,
    /// Each sandbox kept here that could not be known again, by id, until it is removed: listed
    /// with why, and refused all but its `DELETE`.
    unknown: Mutex<HashMap<String, UnknownSandbox>>,
    /// The image jobs, by id, and what cancels each, until each is removed or a while after
    /// it ends.
    image_jobs: Mutex<HashMap<String, (Arc<ImageJobOf>, Stop)>>,
    /// Set once its project is to be removed: no sandbox or image job is added after.
    closed: AtomicBool,
    /// Where its project's data is kept beyond this machine, once it is given one
    /// ([`Home::backed_by`]): its sandboxes and its files take it from here.
    backing: Backing,
}

impl Home {
    /// The images in `images`, which `registry` answers for, and the volumes and the
    /// sandboxes' upper disks under `root`, with each sandbox a server before it kept there
    /// known again, parked ([`Sandbox::kept`]). Its image jobs connect to registries on public
    /// addresses alone, through [`PublicRegistries`] with none named, unless the [`Access`] it
    /// is made for says otherwise ([`Access::through`]). `registry` is this home's alone: it is
    /// backed as the home is ([`backed_by`](Home::backed_by)). Blocking work, as knowing each
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
        // One with the registry's: its images are kept where the rest of the home is.
        let backing = registry.backing().clone();
        let kept = Sandbox::kept(&images, root, &volumes, vm.clone(), park_after(), &backing);
        let unknown = kept
            .unknown
            .into_iter()
            .map(|unknown| (unknown.id().to_owned(), unknown))
            .collect();
        let sandboxes = kept
            .known
            .into_iter()
            .map(|sandbox| (sandbox.id().to_owned(), sandbox))
            .collect();
        Ok(Home {
            images,
            registry,
            registries: Arc::new(PublicRegistries::default()),
            mirror: None,
            public: None,
            files: Files::new(volumes.clone()).backed_by(backing.clone()),
            root: root.to_owned(),
            volumes,
            vm,
            sandboxes: Mutex::new(sandboxes),
            unknown: Mutex::new(unknown),
            naming: Mutex::default(),
            image_jobs: Mutex::default(),
            closed: AtomicBool::default(),
            backing,
        })
    }

    /// This, its project's data kept beyond this machine by `backend` as well, its own stores
    /// the backend's cache from now on: each of its sandboxes, those known again as it was made
    /// among them, takes its disk and volumes from the backend as its machine boots, its
    /// volumes and their files are made, listed, read, written and removed through it, and its
    /// registry lists the tags kept there and brings a manifest it is asked for here. Said
    /// once, as it is made, before any request reaches it. A home given none -- a server on its
    /// own -- keeps nothing beyond its stores. See [`StorageBackend`].
    ///
    /// # Panics
    ///
    /// Where it has a backend already, given to it or to the registry it was made with: a home
    /// keeps the one it was given first.
    pub fn backed_by(self, backend: Arc<dyn StorageBackend>) -> Home {
        assert!(
            self.backing.set(backend),
            "a home is backed once, and this one or its registry has a backend already"
        );
        self
    }

    /// Its volumes, as its own store has them.
    pub fn volumes(&self) -> &VolumeStore {
        &self.volumes
    }

    /// Take in sandbox `id`, whose record and disk are under this home's root already as a
    /// server leaves them -- `sandbox/<id>.json` and `sandbox/<id>.ext4`, as [`let_go`] left
    /// them here or on another server, or as a backup kept them -- as a restart knows a sandbox
    /// again ([`Sandbox::take_in`]): parked, its machine booted as it is next used, behind the
    /// port URLs it was made with. Its image is wanted from the backend first, where the home
    /// has one. Answered as `GET /sandboxes/{id}` answers it. Refused `409` `exists` for an id
    /// a sandbox here has, `404` where no sandbox `id` is kept so, `403` once its project is
    /// being removed, and as making it is where it cannot be known, its record and disk left
    /// as they are.
    ///
    /// [`let_go`]: Home::let_go
    pub async fn take_in(&self, id: &str) -> Result<virtx_core::Sandbox, Refused> {
        let here = self.sandboxes.lock().unwrap().contains_key(id)
            || self.unknown.lock().unwrap().contains_key(id);
        if here {
            return Err(refused(
                StatusCode::CONFLICT,
                "exists",
                format!("a sandbox here has the id {id}"),
            ));
        }
        let sandbox = Sandbox::take_in(
            &self.images,
            &self.root,
            &self.volumes,
            self.vm.clone(),
            park_after(),
            &self.backing,
            id,
        )
        .await?;
        let closed = {
            let mut sandboxes = self.sandboxes.lock().unwrap();
            let closed = self.closed.load(Ordering::SeqCst);
            if !closed {
                sandboxes.insert(id.to_owned(), Arc::clone(&sandbox));
            }
            closed
        };
        if closed {
            tokio::task::spawn_blocking(move || sandbox.leave()).await?;
            return Err(forbidden("this project is being removed"));
        }
        Ok(sandbox.answer())
    }

    /// A sandbox made of `args` under the id `given` it, rather than one picked here: for a
    /// server built on this one that names its sandboxes itself, as one that moves them between
    /// servers does, each keeping the id its client holds. Made and answered as
    /// `POST /sandboxes/` makes and answers one. Refused `400` for an id that cannot be one;
    /// `409` `exists` for an id a sandbox here has, has made or keeps -- one let go of here is
    /// [taken in](Home::take_in) instead; and as `POST /sandboxes/` is otherwise.
    pub async fn make(
        &self,
        args: SandboxCreateArgs,
        given: Given,
    ) -> Result<virtx_core::Sandbox, Refused> {
        self.create(args, Some(&given.id)).await
    }

    /// A sandbox made of `args`, under `id` where it is given, and kept past this process.
    async fn create(
        &self,
        args: SandboxCreateArgs,
        id: Option<&str>,
    ) -> Result<virtx_core::Sandbox, Refused> {
        let _naming = self.naming(args.name.as_deref())?;
        let sandbox = Sandbox::create(
            &self.images,
            &self.root,
            &self.volumes,
            &args,
            self.vm.clone(),
            park_after(),
            id,
            true,
            &self.backing,
        )
        .await?;

        let id = sandbox.id().to_owned();
        let answer = sandbox.answer();
        let closed = {
            let mut sandboxes = self.sandboxes.lock().unwrap();
            let closed = self.closed.load(Ordering::SeqCst);
            if !closed {
                sandboxes.insert(id.clone(), Arc::clone(&sandbox));
            }
            closed
        };
        if closed {
            tokio::task::spawn_blocking(move || sandbox.close()).await?;
            if let Some(backend) = self.backing.get() {
                let _ = backend.remove(Stored::Disk(&id)).await;
            }
            return Err(forbidden("this project is being removed"));
        }
        Ok(answer)
    }

    /// Let go of sandbox `id` here, as this server lets go of each as it ends
    /// ([`Sandbox::leave`]): its machine stopped if it is up, as parking stops it, and its
    /// record and disk left under this home's root, for a server to [take in](Home::take_in) --
    /// another one it is moved to, this one again, or a backup's. Nothing of it is removed from
    /// the backend, where the home has one: what is kept of it is the embedder's from then on,
    /// to take in or to remove there itself. From then on no request here reaches it. Refused
    /// `409` `in_use` while it is in use -- a job runs in it, or something reaches it -- and
    /// `404` for an id no sandbox here has. One kept here and not known again is let go of as
    /// it is. Blocks until its machine has stopped.
    pub async fn let_go(&self, id: &str) -> Result<(), Refused> {
        if self.unknown.lock().unwrap().remove(id).is_some() {
            return Ok(());
        }
        let left = {
            let mut sandboxes = self.sandboxes.lock().unwrap();
            let held = sandboxes
                .get(id)
                .ok_or_else(|| not_found(format!("no sandbox here has the id {id}")))?;
            if held.idle().is_none() {
                return Err(refused(
                    StatusCode::CONFLICT,
                    "in_use",
                    format!("sandbox {id} is in use: a job runs in it, or it is reached"),
                ));
            }
            sandboxes.remove(id)
        };
        if let Some(sandbox) = left {
            tokio::task::spawn_blocking(move || sandbox.leave()).await?;
        }
        Ok(())
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

    /// This, its anonymous pulls -- a pull, a build's base -- shared through `public`: an image
    /// it has is fetched from there and not pulled, and one pulled is told it. None is nothing
    /// shared, as a home has by default. A [`Homes`](crate::access::Homes) that makes homes for
    /// many projects shares public images between them so. See
    /// [`RegistryClient::with_public`].
    pub fn sharing(mut self, public: Option<Arc<dyn PublicImages>>) -> Home {
        self.public = public;
        self
    }

    pub fn images(&self) -> &ImageStore {
        &self.images
    }

    /// Delete the snapshots its machines no longer need, as [`Sandbox::sweep_snapshots`] says.
    /// Blocking work.
    pub fn sweep_snapshots(&self) {
        Sandbox::sweep_snapshots(&self.root, &self.vm);
    }

    /// What answers this home's `/images/oci/v2/`.
    pub fn registry(&self) -> &Arc<RegistryServer> {
        &self.registry
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

    /// Close it as [`close`](Home::close) does, whatever lives here, and close every sandbox
    /// as [`close_all`](Home::close_all) does. Each kept here and not known again is let go
    /// of, its disk and record left where they are.
    pub(crate) fn forget(&self) {
        {
            let _sandboxes = self.sandboxes.lock().unwrap();
            self.closed.store(true, Ordering::SeqCst);
        }
        self.close_all();
        self.unknown.lock().unwrap().clear();
    }

    /// Cancel every image job here, and close every sandbox, as removing each does, side by
    /// side: blocks until each machine has stopped and synced what it wrote.
    pub fn close_all(&self) {
        for (_, (job, stop)) in self.image_jobs.lock().unwrap().drain() {
            cancel_now(&job, &stop);
        }
        let closed: Vec<_> = self
            .sandboxes
            .lock()
            .unwrap()
            .drain()
            .map(|(_, held)| held)
            .collect();
        std::thread::scope(|scope| {
            for sandbox in &closed {
                scope.spawn(|| sandbox.close());
            }
        });
    }

    /// Take `name` for a sandbox being made, until what this answers drops: refused `409`
    /// `exists` where a sandbox here has it, or one being made has taken it.
    fn naming(&self, name: Option<&str>) -> Result<Naming<'_>, Refused> {
        let Some(name) = name else {
            return Ok(Naming(self, None));
        };
        let sandboxes = self.sandboxes.lock().unwrap();
        let mut naming = self.naming.lock().unwrap();
        let held = sandboxes.values().any(|held| held.name() == Some(name))
            || (self.unknown.lock().unwrap().values()).any(|kept| kept.name() == Some(name));
        if held || !naming.insert(name.to_owned()) {
            return Err(refused(
                StatusCode::CONFLICT,
                "exists",
                format!("a sandbox here is named {name} already"),
            ));
        }
        Ok(Naming(self, Some(name.to_owned())))
    }

    /// Its sandboxes, whether each one's machine is up or not: for a server's own routes
    /// ([`serve::router`](crate::serve::router)'s `own`) and its work between requests, which
    /// may find, count or park them.
    pub fn sandboxes(&self) -> Sandboxes<'_> {
        Sandboxes(self)
    }

    /// Park every sandbox here that nothing uses now ([`Sandbox::park`]), however recently it
    /// was used, side by side; those in use are left as they are. These are the sandboxes here
    /// as it is called: one made meanwhile is left up, so a server that drains its host stops
    /// sending it work first.
    pub async fn park_unused(&self) {
        let parked = self
            .sandboxes()
            .list()
            .into_iter()
            .map(|sandbox| async move {
                let _ = sandbox.park().await;
            });
        futures_util::future::join_all(parked).await;
    }

    /// Park every sandbox here, in use or not ([`Sandbox::park_anyway`]), side by side: what
    /// runs in each ends, as cancelling it does, and each is kept, to boot again when it is next
    /// used. As [`park_unused`](Home::park_unused), the sandboxes here as it is called.
    pub async fn park_all(&self) {
        let parked = self
            .sandboxes()
            .list()
            .into_iter()
            .map(|sandbox| async move {
                sandbox.park_anyway().await;
            });
        futures_util::future::join_all(parked).await;
    }

    /// Cancel every image job here, and let go of every sandbox as this server ends, side by
    /// side, each kept for the next server over this home ([`Sandbox::leave`]): blocks until
    /// each machine has stopped and synced what it wrote.
    pub fn leave_all(&self) {
        for (_, (job, stop)) in self.image_jobs.lock().unwrap().drain() {
            cancel_now(&job, &stop);
        }
        let left: Vec<_> = self
            .sandboxes
            .lock()
            .unwrap()
            .drain()
            .map(|(_, held)| held)
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
            return Ok(Arc::clone(held));
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

/// A home's sandboxes, as each is when it is asked: those the home knows, each whether its
/// machine is up or not ([`Home::sandboxes`]). One kept in the home and not known again, which
/// takes only its `DELETE`, is none of them, and is [counted](Sandboxes::counts) apart.
pub struct Sandboxes<'a>(&'a Home);

/// How many sandboxes a home holds, as [`Sandboxes::counts`] answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Counts {
    /// Those it knows.
    pub known: usize,
    /// Of those, the ones whose machine is up.
    pub up: usize,
    /// Those kept in it and not known again, each holding its disk and record under the home's
    /// root until it is removed.
    pub unknown: usize,
}

impl Sandboxes<'_> {
    /// Each of them, in no set order.
    pub fn list(&self) -> Vec<Arc<Sandbox>> {
        let sandboxes = self.0.sandboxes.lock().unwrap();
        sandboxes.values().map(Arc::clone).collect()
    }

    /// The sandbox `id`, if it is one of them.
    pub fn get(&self, id: &str) -> Option<Arc<Sandbox>> {
        let sandboxes = self.0.sandboxes.lock().unwrap();
        sandboxes.get(id).map(Arc::clone)
    }

    /// How many there are, how many of those have their machine up now, and how many more the
    /// home holds and does not know.
    pub fn counts(&self) -> Counts {
        let (known, up) = {
            let sandboxes = self.0.sandboxes.lock().unwrap();
            let up = sandboxes.values().filter(|held| held.is_up());
            (sandboxes.len(), up.count())
        };
        let unknown = self.0.unknown.lock().unwrap().len();
        Counts { known, up, unknown }
    }

    /// The sandbox `id`, if it is one of them and its machine is up now: what writes its disk
    /// at the moment it is asked. Nothing holds it up after, and a park or a boot may change
    /// the answer as soon as it is given.
    pub fn running_on(&self, id: &str) -> Option<Arc<Sandbox>> {
        self.get(id).filter(|sandbox| sandbox.is_up())
    }
}

/// A name a sandbox being made has taken, given back as this drops: once the sandbox is among
/// [`Home`]'s, which holds it from there, or once its making failed.
struct Naming<'a>(&'a Home, Option<String>);

impl Drop for Naming<'_> {
    fn drop(&mut self) {
        if let Some(name) = &self.1 {
            self.0.naming.lock().unwrap().remove(name);
        }
    }
}

/// The id a sandbox is made under, chosen by a server built on this one rather than picked
/// here ([`Home::make`]).
pub struct Given {
    /// Letters, digits, `_` and `-`, the first a letter or digit, 64 at most.
    pub id: String,
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
        .route(
            "/sandboxes/{id}",
            get(sandbox_get)
                .patch(sandbox_resize)
                .delete(sandbox_remove),
        )
        .route("/sandboxes/{id}/ports/{port}", get(sandbox_port))
        .route(
            "/sandboxes/{id}/client-ports/{port}",
            get(sandbox_client_port),
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
/// ends or is cancelled, the image it makes then tagged `tags` where it names one, and kept
/// [`IMAGE_JOB_KEPT`] after. Its tag's change and its end run to their end once begun, a cancel
/// waiting for them ([`cancel`]); a job cancelled first leaves its tag be.
fn image_job<Fut>(
    home: &Arc<Home>,
    kind: ImageJobKind,
    reference: String,
    tags: Option<Reference>,
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
    let gate = Arc::new(tokio::sync::Mutex::new(false));
    let (closed, backing, images) = (Arc::clone(&gate), home.backing.clone(), home.images.clone());
    let task = tokio::spawn(async move {
        let worked = work.await;
        let ending = Arc::clone(&ended);
        let ending = tokio::spawn(async move {
            let cancelled = closed.lock().await;
            if *cancelled {
                return false;
            }
            let done = match (worked, tags) {
                (Ok(digest), Some(reference)) => {
                    (tag(&backing, &images, &reference, &digest).await).map(|()| digest)
                }
                (worked, _) => worked,
            };
            let end = match done {
                Ok(digest) => ImageJobEnd {
                    digest: Some(digest.to_string()),
                    error: None,
                },
                Err(e) => ImageJobEnd {
                    digest: None,
                    error: Some(failed(e)),
                },
            };
            ending.end(
                |job| {
                    job.running = false;
                    job.digest.clone_from(&end.digest);
                    job.error.clone_from(&end.error);
                },
                ImageJobEntry::End(end.clone()),
            );
            true
        });
        let ended_here =
            (ending.await).unwrap_or_else(|e| std::panic::resume_unwind(e.into_panic()));
        if ended_here {
            tokio::time::sleep(IMAGE_JOB_KEPT).await;
            kept.image_jobs.lock().unwrap().remove(&removed);
            ended.remove();
        }
    });
    let stop = Stop {
        task: task.abort_handle(),
        gate,
    };
    jobs.insert(id.clone(), (Arc::clone(&job), stop));
    Ok(created(
        format!("/images/jobs/{id}"),
        &*job.job.lock().unwrap(),
    ))
}

/// Why an image job failed with `e`, as the job says it: a failure on the server's files by its
/// kind, and a machine that did not start as its process told it, with the account of either,
/// which can name the server's paths, on the server's stderr; the rest as it says itself.
fn failed(e: Failure) -> String {
    let e = match e.downcast::<BuildError>() {
        Ok(e) => {
            if let BuildError::Io { source, .. }
            | BuildError::Fstree(FstreeError::Io { source, .. }) = &*e
            {
                return io_failure(source, &e);
            }
            return match *e {
                BuildError::Store(e) => Refused::from(e).2,
                BuildError::Machine(e) => Refused::from(e).2,
                e => e.to_string(),
            };
        }
        Err(e) => e,
    };
    let e = match e.downcast::<RegistryError>() {
        Ok(e) => {
            if let RegistryError::Fstree(FstreeError::Io { source, .. }) = &*e {
                return io_failure(source, &e);
            }
            return match *e {
                RegistryError::Store(e) => Refused::from(e).2,
                e => e.to_string(),
            };
        }
        Err(e) => e,
    };
    match e.downcast::<ImageStoreError>() {
        Ok(e) => Refused::from(*e).2,
        Err(e) => e.to_string(),
    }
}

async fn image_pull(Extension(home): Extension<Arc<Home>>, Whole(request): Whole) -> Answer {
    let args: ImagePullArgs = body(&request)?;
    let reference = reference(&args.reference)?;
    let (images, registries) = (home.images.clone(), Arc::clone(&home.registries));
    let (mirror, public) = (home.mirror.clone(), home.public.clone());
    image_job(
        &home,
        ImageJobKind::Pull,
        args.reference,
        Some(reference.clone()),
        move |report| async move {
            let registry = RegistryClient::new(images.clone(), args.auth)
                .with_mirror(mirror.as_deref())
                .with_public(public)
                .through(Some(&*registries))?;
            let (_, image) = registry.pull(&reference, report).await?;
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
        None,
        move |report| async move {
            let registry = RegistryClient::new(images, args.auth).through(Some(&*registries))?;
            Ok(registry.push(&reference, report).await?)
        },
    )
}

/// What a build's `RUN` steps each boot a machine of.
const BUILD_MACHINE: MachineSpec = MachineSpec {
    vcpus: DEFAULT_VCPUS,
    ram_mib: DEFAULT_MEMORY_MIB,
    gpu: false,
    gpu_memory_mib: None,
    network: true,
};

async fn image_build(Extension(home): Extension<Arc<Home>>, Whole(request): Whole) -> Answer {
    let args: ImageBuildArgs = body(&request)?;
    let reference = reference(&args.reference)?;
    if args.recipe.base != "scratch" {
        self::reference(&args.recipe.base)?;
    }
    let (images, vm) = (home.images.clone(), home.vm.clone());
    let (registries, mirror) = (Arc::clone(&home.registries), home.mirror.clone());
    let public = home.public.clone();
    let recipe = args.recipe;
    // Room for one machine as the build is asked for, if a step of it boots one -- its steps
    // boot one at a time -- so a build there is none for is refused `503` as a sandbox is,
    // rather than failing as a job. It is held from then on, its base's pull and the steps
    // before its first `RUN` included.
    let boots = recipe.steps.iter().any(|step| matches!(step, Step::Run(_)));
    let room = match boots {
        true => virtx_uvm::admit(&BUILD_MACHINE)?,
        false => None,
    };
    image_job(
        &home,
        ImageJobKind::Build,
        args.reference,
        Some(reference),
        move |mut report| async move {
            // Held until the build ends, or is cancelled: its future dropped.
            let _room = room;
            let (mut image, mut layers, _base) = base(
                &images,
                &*registries,
                mirror.as_deref(),
                public,
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
                    &BUILD_MACHINE,
                    &mut report,
                )
                .await?;
            }
            let store = images.clone();
            let digest =
                tokio::task::spawn_blocking(move || store.insert_image(&image, &layers)).await??;
            Ok(digest)
        },
    )
}

/// `reference` made to name `image` in `images`, and kept so beyond this machine where the
/// home is backed ([`Backing::tag`]): an image job's last step.
async fn tag(
    backing: &Backing,
    images: &ImageStore,
    reference: &Reference,
    image: &Digest,
) -> Result<(), Failure> {
    match backing.tag(images, reference, image).await {
        Ok(()) => Ok(()),
        Err(TagError::Store(e)) => Err(e.into()),
        Err(TagError::Kept(e)) => {
            eprintln!("keeping {reference} beyond this machine: {e:#}");
            Err("the tag could not be kept beyond this machine".into())
        }
    }
}

/// A build's base: its config and layers, and its image held for as long as the build runs.
/// Nothing for `scratch`, as in a Dockerfile; else the image tagged `base` here, or the one
/// pulled into the store now, anonymously, through `registries`, from `mirror` first if it is
/// Docker Hub's and shared through `public`, and left untagged: a build tags only what it
/// builds.
/// Its config names it as what the build is based on ([`based_on`]).
async fn base(
    images: &ImageStore,
    registries: &dyn RegistryProxy,
    mirror: Option<&str>,
    public: Option<Arc<dyn PublicImages>>,
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
                .with_public(public)
                .through(Some(registries))?
                .pull(&reference, report)
                .await?
                .1
        }
    };
    let held = images.hold_image(&image)?;
    let (mut config, layers) = images
        .get_image(&image)?
        .ok_or_else(|| ImageStoreError::Missing(image.clone()))?;
    based_on(&mut config, base, &image);
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
    let (job, stop) =
        removed.ok_or_else(|| not_found(format!("no image job here has the id {id}")))?;
    cancel(&job, &stop).await;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// What stops an image job: its task, and the gate its tag's change and its end pass, closed
/// (`true`) once it is cancelled.
struct Stop {
    task: AbortHandle,
    gate: Arc<tokio::sync::Mutex<bool>>,
}

/// Stop `job`, leaving no tag changed, end it as cancelled, and remove its log. One whose tag's
/// change is under way is waited for and ends as that does, the cancel coming after it.
async fn cancel(job: &ImageJobOf, stop: &Stop) {
    stop.task.abort();
    *stop.gate.lock().await = true;
    end_cancelled(job);
}

/// [`cancel`], as its home closes: a tag's change under way goes on, not waited for.
fn cancel_now(job: &ImageJobOf, stop: &Stop) {
    stop.task.abort();
    if let Ok(mut gate) = stop.gate.try_lock() {
        *gate = true;
    }
    end_cancelled(job);
}

/// End `job` as cancelled, if it has not ended, and remove its log.
fn end_cancelled(job: &ImageJobOf) {
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
    let pruned = tokio::task::spawn_blocking(move || {
        let pruned = home.images.prune(PRUNE_GRACE);
        // A snapshot of an image pruned is restored from no more.
        Sandbox::sweep_snapshots(&home.root, &home.vm);
        pruned
    })
    .await??;
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

/// The volumes kept beyond this machine, where the home's backend lists them.
async fn kept_volumes(home: &Home) -> Result<Option<Vec<VolumeListOutputItem>>, Refused> {
    match home.backing.get() {
        Some(backend) => backend.volumes().await.map_err(storage_failed),
        None => Ok(None),
    }
}

async fn volume_list(Extension(home): Extension<Arc<Home>>) -> Answer {
    if let Some(kept) = kept_volumes(&home).await? {
        return Ok(json(&kept));
    }
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
    // Kept beyond this machine from now on: one of its name kept already is refused, and this
    // one removed.
    if let Some(backend) = home.backing.get()
        && let Err(e) = backend.made(Stored::Volume(&name), 0).await
    {
        let _ = home.volumes.remove(&name);
        return Err(e.into());
    }
    Ok(created(format!("/volumes/{name}"), &volume(&home, name)?))
}

async fn volume_get(
    Extension(home): Extension<Arc<Home>>,
    Segments(name): Segments<String>,
) -> Answer {
    if let Some(kept) = kept_volumes(&home).await? {
        let found = kept.into_iter().find(|volume| volume.name == name);
        return Ok(json(&found.ok_or(VolumeError::Unknown(name))?));
    }
    Ok(json(&volume(&home, name)?))
}

/// Remove it, and what is kept of it beyond this machine first: refused before anything is
/// removed while a sandbox here mounts it, or the backend refuses.
async fn volume_remove(
    Extension(home): Extension<Arc<Home>>,
    Segments(name): Segments<String>,
) -> Answer {
    let Some(backend) = home.backing.get() else {
        home.volumes.remove(&name)?;
        return Ok(StatusCode::NO_CONTENT.into_response());
    };
    // Held alone until it is removed here, so no sandbox mounts it meanwhile.
    let alone = match home.volumes.open(&name) {
        Ok(volume) => Some(volume.hold_alone()?),
        Err(VolumeError::Unknown(_)) => None,
        Err(e) => return Err(e.into()),
    };
    match backend.remove(Stored::Volume(&name)).await {
        Ok(()) => {}
        Err(Refusal::Unknown(_)) if alone.is_some() => {}
        Err(e) => return Err(e.into()),
    }
    if let Some(alone) = alone {
        home.volumes.remove_held(&name, alone)?;
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// `name` brought up to what is kept beyond this machine, where it is, to be read here.
async fn bring_up(home: &Home, name: &str) -> Result<(), Refused> {
    if let Some(backend) = home.backing.get() {
        backend.bring_up(Stored::Volume(name)).await?;
    }
    Ok(())
}

/// `name` held where it is kept beyond this machine, if it is, brought up to what is kept
/// there, to be written here until what this answers drops.
async fn take(home: &Home, name: &str) -> Result<Option<virtx_uvm::backend::Held>, Refused> {
    match home.backing.get() {
        Some(backend) => Ok(Some(backend.take(Stored::Volume(name)).await?)),
        None => Ok(None),
    }
}

async fn volume_mount_list(
    Extension(home): Extension<Arc<Home>>,
    Segments(name): Segments<String>,
) -> Answer {
    bring_up(&home, &name).await?;
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
    let _taken = take(&home, &name).await?;
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
    let _taken = take(&home, &name).await?;
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

/// Every sandbox; with `?name=`, the one named so, if any.
async fn sandbox_list(Extension(home): Extension<Arc<Home>>, RawQuery(query): RawQuery) -> Answer {
    let name = query.as_deref().and_then(|query| {
        form_urlencoded::parse(query.as_bytes())
            .find(|(key, _)| key == "name")
            .map(|(_, name)| name.into_owned())
    });
    let sandboxes = home.sandboxes.lock().unwrap();
    let mut listed: Vec<_> = sandboxes
        .values()
        .filter(|held| name.is_none() || held.name() == name.as_deref())
        .map(|held| held.answer())
        .collect();
    let unknown = home.unknown.lock().unwrap();
    listed.extend(
        unknown
            .values()
            .filter(|kept| name.is_none() || kept.name() == name.as_deref())
            .map(UnknownSandbox::answer),
    );
    Ok(json(&listed))
}

async fn sandbox_create(Extension(home): Extension<Arc<Home>>, Whole(request): Whole) -> Answer {
    let args: SandboxCreateArgs = body(&request)?;

    let answer = home.create(args, None).await?;
    Ok(created(format!("/sandboxes/{}", answer.id), &answer))
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

/// `PATCH /sandboxes/{id}`: its vCPUs and memory from its VM's next boot on, its VM parked now
/// if it is up and nothing uses it, and refused `409`, `in_use`, if something does; its disk
/// grown, never shrunk, as its VM next boots, without parking it.
async fn sandbox_resize(
    Extension(home): Extension<Arc<Home>>,
    Segments(id): Segments<String>,
    Whole(request): Whole,
) -> Answer {
    let args: SandboxResizeArgs = body(&request)?;
    Ok(json(&home.sandbox(&id)?.resize(&args).await?))
}

/// Remove it, and its disk kept beyond this machine first: refused, and nothing removed, where
/// the backend refuses.
async fn sandbox_remove(
    Extension(home): Extension<Arc<Home>>,
    Segments(id): Segments<String>,
) -> Answer {
    let here = home.sandboxes.lock().unwrap().contains_key(&id)
        || home.unknown.lock().unwrap().contains_key(&id);
    if !here {
        return Err(not_found(format!("no sandbox here has the id {id}")));
    }
    if let Some(backend) = home.backing.get() {
        match backend.remove(Stored::Disk(&id)).await {
            Ok(()) | Err(Refusal::Unknown(_)) => {}
            Err(e) => return Err(e.into()),
        }
    }
    let unknown = home.unknown.lock().unwrap().remove(&id);
    if let Some(unknown) = unknown {
        tokio::task::spawn_blocking(move || unknown.remove()).await?;
        return Ok(StatusCode::NO_CONTENT.into_response());
    }
    let removed = home.sandboxes.lock().unwrap().remove(&id);
    let removed = removed.ok_or_else(|| not_found(format!("no sandbox here has the id {id}")))?;
    tokio::task::spawn_blocking(move || removed.close()).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// How often a port on local's WebSocket is pinged while no connection comes, so that what stands
/// between it and its client does not take it for idle.
const CLIENT_PORT_KEEPALIVE: Duration = Duration::from_secs(15);

/// How long a port on local's WebSocket has to answer the connection said on it, longer than a
/// client takes to reach its target, before the connection is refused.
const CLIENT_PORT_ANSWER: Duration = Duration::from_secs(20);

/// The WebSocket subprotocol a port's TCP, and a port on local's, is carried in.
const TCP_SUBPROTOCOL: &str = "virtx.tcp";

/// How much of a port's TCP one message carries at most.
const CHUNK: usize = 64 << 10;

/// The WebSocket of `virtx.tcp` a request to a port, in the sandbox or on local, opens; or what it is
/// answered otherwise: `426` unless it opens a WebSocket, `400` unless of the subprotocol.
fn tcp_websocket(
    upgrade: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
    headers: &HeaderMap,
) -> Result<WebSocketUpgrade, Box<Response>> {
    let Ok(upgrade) = upgrade else {
        return Err(Box::new(StatusCode::UPGRADE_REQUIRED.into_response()));
    };
    let offered = headers
        .get(header::SEC_WEBSOCKET_PROTOCOL)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(',')
                .any(|offered| offered.trim() == TCP_SUBPROTOCOL)
        });
    if !offered {
        let refusal = invalid(format!(
            "a port is reached over the WebSocket subprotocol {TCP_SUBPROTOCOL}"
        ));
        return Err(Box::new(refusal.into_response()));
    }
    Ok(upgrade.protocols([TCP_SUBPROTOCOL]))
}

/// `GET /sandboxes/{id}/ports/{port}`, a WebSocket of `virtx.tcp` carrying a TCP connection to
/// the port in the sandbox: closed at once, saying why, when the port is not reached, such as
/// while no machine runs. Refused as [`tcp_websocket`] says, and `404` for a port the sandbox
/// was not made with.
async fn sandbox_port(
    Extension(home): Extension<Arc<Home>>,
    Segments((id, port)): Segments<(String, u16)>,
    headers: HeaderMap,
    upgrade: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Answer {
    let sandbox = home.sandbox(&id)?;
    let opened =
        (sandbox.args().ports.iter()).any(|spec| spec.on == PortSide::Sandbox && spec.port == port);
    if !opened {
        return Err(not_found(format!(
            "sandbox {id} was made with no port {port}"
        )));
    }
    let upgrade = match tcp_websocket(upgrade, &headers) {
        Ok(upgrade) => upgrade,
        Err(refused) => return Ok(*refused),
    };
    let activity = sandbox.activity();
    Ok(upgrade.on_upgrade(move |mut socket| async move {
        match activity.sandbox().connect(port).await {
            Ok(reached) => carry(socket, reached).await,
            Err(e) => {
                let frame = CloseFrame {
                    code: close_code::ERROR,
                    reason: close_reason(&e.to_string()).into(),
                };
                let _ = socket.send(Message::Close(Some(frame))).await;
            }
        }
    }))
}

/// Carry bytes between `socket` and `reached` until either ends: what is read from `reached` as
/// binary messages, and the binary messages to it.
async fn carry(mut socket: WebSocket, mut reached: TcpStream) {
    let (mut from, mut to) = reached.split();
    let mut buffer = vec![0u8; CHUNK];
    loop {
        tokio::select! {
            read = from.read(&mut buffer) => match read {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let message = Message::Binary(buffer[..n].to_vec().into());
                    if socket.send(message).await.is_err() {
                        break;
                    }
                }
            },
            received = socket.recv() => match received {
                Some(Ok(Message::Binary(bytes))) => {
                    if to.write_all(&bytes).await.is_err() {
                        break;
                    }
                }
                // Answered by the socket itself.
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                // Text is no TCP's, and anything else is the end.
                Some(Ok(Message::Text(_) | Message::Close(_)) | Err(_)) | None => break,
            },
        }
    }
    let _ = socket.send(Message::Close(None)).await;
}

/// `said` as a close's reason, which a WebSocket holds 123 bytes of: cut at a character's end.
fn close_reason(said: &str) -> String {
    let mut end = said.len().min(123);
    while !said.is_char_boundary(end) {
        end -= 1;
    }
    said[..end].to_owned()
}

/// `GET /sandboxes/{id}/client-ports/{port}`, a WebSocket of `virtx.tcp`: the port taken by
/// this client while it is open, and the next connection a process makes to it carried in it.
/// The connection is said with an empty binary message; the client answers it with another once
/// it has reached what the port is, and the process's `connect` succeeds then, or closes the
/// WebSocket, which refuses it. Refused as [`tcp_websocket`] says, and `404` for a port the
/// sandbox was not made with: before the port is taken, so a request refused takes no
/// connection.
async fn sandbox_client_port(
    Extension(home): Extension<Arc<Home>>,
    Segments((id, port)): Segments<(String, u16)>,
    headers: HeaderMap,
    upgrade: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Answer {
    let upgrade = match tcp_websocket(upgrade, &headers) {
        Ok(upgrade) => upgrade,
        Err(refused) => return Ok(*refused),
    };
    let sandbox = home.sandbox(&id)?;
    let taker = sandbox.take_client_port(port)?;
    Ok(upgrade.on_upgrade(move |socket| take_client_port(socket, sandbox, taker)))
}

/// Wait on `socket` for the next connection `taker` has, and carry it if the client reaches its
/// target in [`CLIENT_PORT_ANSWER`]. A client that closes `socket` meanwhile, or answers no ping
/// before the next, takes none.
async fn take_client_port(mut socket: WebSocket, sandbox: Arc<Sandbox>, taker: ClientPortTaker) {
    let mut pinged = false;
    let connection = loop {
        tokio::select! {
            connection = taker.next() => match connection {
                Some(connection) => break connection,
                None => {
                    let _ = socket.send(Message::Close(None)).await;
                    return;
                }
            },
            received = socket.recv() => match received {
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => pinged = false,
                _ => return,
            },
            () = tokio::time::sleep(CLIENT_PORT_KEEPALIVE) => {
                if pinged || socket.send(Message::Ping(Bytes::new())).await.is_err() {
                    return;
                }
                pinged = true;
            }
        }
    };
    drop(taker);
    if socket.send(Message::Binary(Bytes::new())).await.is_err() {
        return;
    }
    // Dropped unanswered, the connection is refused.
    let answered = tokio::time::timeout(CLIENT_PORT_ANSWER, async {
        loop {
            match socket.recv().await {
                Some(Ok(Message::Binary(said))) if said.is_empty() => return true,
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                _ => return false,
            }
        }
    });
    if answered.await != Ok(true) {
        return;
    }
    let _activity = sandbox.activity();
    let Ok(reached) = connection.accept().await else {
        return;
    };
    carry_halves(socket, reached).await;
}

/// Carry bytes between `socket` and `reached` until both directions have ended: what is read
/// from `reached` as binary messages, and its end as an empty one; the binary messages to it, and
/// an empty one as the client's end, `reached`'s writing shut. Ended at once by a failure, or a
/// close.
async fn carry_halves(mut socket: WebSocket, mut reached: TcpStream) {
    let (mut from, mut to) = reached.split();
    let mut buffer = vec![0u8; CHUNK];
    let (mut read_all, mut written_all) = (false, false);
    while !(read_all && written_all) {
        tokio::select! {
            read = from.read(&mut buffer), if !read_all => match read {
                Ok(n) => {
                    read_all = n == 0;
                    let message = Message::Binary(buffer[..n].to_vec().into());
                    if socket.send(message).await.is_err() {
                        break;
                    }
                }
                Err(_) => break,
            },
            received = socket.recv() => match received {
                Some(Ok(Message::Binary(bytes))) if bytes.is_empty() => {
                    written_all = true;
                    let _ = to.shutdown().await;
                }
                Some(Ok(Message::Binary(bytes))) => {
                    if to.write_all(&bytes).await.is_err() {
                        break;
                    }
                }
                // Answered by the socket itself.
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                // Text is no TCP's, and anything else is the end.
                _ => break,
            },
        }
    }
    let _ = socket.send(Message::Close(None)).await;
}

async fn sandbox_job_list(
    Extension(home): Extension<Arc<Home>>,
    Segments(id): Segments<String>,
) -> Answer {
    Ok(json(&home.sandbox(&id)?.jobs()))
}

/// `POST /sandboxes/{id}/jobs/exec/`, answered with the job -- or, to a request that accepts
/// `text/event-stream`, `201` with the job as the first event and then its log, and its path in
/// `Location`: its output and end with no second request.
async fn sandbox_exec(
    Extension(home): Extension<Arc<Home>>,
    Segments(id): Segments<String>,
    headers: HeaderMap,
    Whole(request): Whole,
) -> Answer {
    let args: SandboxExecArgs = body(&request)?;
    if args.cmd.is_empty() {
        return Err(invalid("cmd: an exec names a command"));
    }
    let sandbox = home.sandbox(&id)?;
    let timeout = args.timeout_ms.map(Duration::from_millis);
    let job = sandbox.exec(args.cmd, args.stdin, timeout).await?;
    let answered = job.get();
    let location = format!("/sandboxes/{id}/jobs/{}", answered.id);
    if !accepts_event_stream(&headers) {
        return Ok(created(location, &answered));
    }
    let log = Arc::clone(job.log()).events_led_by(&SandboxExecItem::Job(answered));
    Ok((
        StatusCode::CREATED,
        [
            (header::LOCATION, location),
            (header::CONTENT_TYPE, "text/event-stream".to_owned()),
        ],
        Body::from_stream(log),
    )
        .into_response())
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

/// The seconds a write to a job's stdin turned away for now asks to wait: room comes as its
/// process reads.
const STDIN_FULL_RETRY: u16 = 1;

/// Write to the stdin of a job started with one, and close it if `eof`: answered once it is in
/// the process's stdin, which takes a pipe's worth before the process reads any. Refused `409`,
/// `ended`, for one started without, one whose stdin is closed, one that has ended, and a write
/// its process closed its stdin under, which says how much of it went in. Turned away for now,
/// `503`, `capacity`, where the machine already holds as many writes to it as it takes, none of
/// it taken.
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

/// A request's body, whole: refused `413` `too_large` past [`BODY_LIMIT`], and `400` `invalid` if
/// it broke off, as any refusal here is, in the protocol's envelope rather than as text.
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

/// A path's segments read as a `T`, as axum's `Path` reads them: refused `400` `invalid` when they
/// do not read as one, in the protocol's envelope and the server's words.
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

/// A refusal: its status, `code` and `message`, the members its error has beside them, and the
/// seconds its `Retry-After` asks, if any.
pub struct Refused(
    StatusCode,
    &'static str,
    String,
    serde_json::Map<String, serde_json::Value>,
    Option<u16>,
);

pub fn refused(status: StatusCode, code: &'static str, message: impl Display) -> Refused {
    Refused(
        status,
        code,
        message.to_string(),
        serde_json::Map::new(),
        None,
    )
}

impl Refused {
    /// It with `name` beside its `code` and `message`.
    fn with(mut self, name: &str, value: impl Into<serde_json::Value>) -> Refused {
        self.3.insert(name.to_owned(), value.into());
        self
    }

    /// It asking to be sent again after `seconds`.
    fn after(mut self, seconds: u16) -> Refused {
        self.4 = Some(seconds);
        self
    }

    /// The status it is answered with.
    pub fn status(&self) -> StatusCode {
        self.0
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

/// How long a client is told to wait before it asks again for what found no room.
const CAPACITY_RETRY_AFTER: &str = "5";

impl IntoResponse for Refused {
    fn into_response(self) -> Response {
        let Refused(status, code, message, members, after) = self;
        let mut error = serde_json::json!({"code": code, "message": message});
        error.as_object_mut().unwrap().extend(members);
        let body = serde_json::json!({ "error": error });
        let mut response = (status, json(&body)).into_response();
        if let Some(seconds) = after {
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, seconds.into());
        } else if code == "capacity" {
            let wait = http::HeaderValue::from_static(CAPACITY_RETRY_AFTER);
            response.headers_mut().insert(header::RETRY_AFTER, wait);
        }
        response
    }
}

/// What a refusal says of an operation on the host's files that failed with `e`: `e`'s kind.
/// `message` can name the server's own paths, so it goes to the server's stderr instead
/// ([`HostPaths::Hidden`]).
fn io_failure(e: &std::io::Error, message: impl Display) -> String {
    HostPaths::Hidden.told(e.kind(), message)
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

/// What booting a sandbox's machine, or sending it a command or a signal, fails with. A machine
/// that did not start is answered with why as its process told it, and its whole account, which
/// can name the server's paths, goes to the server's stderr.
impl From<MachineError> for Refused {
    fn from(e: MachineError) -> Refused {
        match &e {
            MachineError::NoCommand => invalid(e),
            MachineError::DidNotStart { account, told } => {
                let _ = writeln!(io::stderr(), "{account}");
                refused(StatusCode::INTERNAL_SERVER_ERROR, "boot_failed", told)
            }
            MachineError::Io { source, .. }
            | MachineError::Fstree(FstreeError::Io { source, .. })
            | MachineError::Volume(VolumeError::Io { source, .. }) => {
                internal(io_failure(source, &e))
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
            SandboxError::Exists(_) => refused(StatusCode::CONFLICT, "exists", e),
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
            // Nothing was taken, and room comes as the process reads: sent again, as any
            // `capacity` is.
            SandboxError::StdinFull => {
                refused(StatusCode::SERVICE_UNAVAILABLE, "capacity", e).after(STDIN_FULL_RETRY)
            }
            SandboxError::Image(e) => e.into(),
            SandboxError::Io { ref source, .. } => internal(io_failure(source, &e)),
            SandboxError::Volume(e) => e.into(),
            SandboxError::Backend(e) => e.into(),
            SandboxError::Machine(e) => e.into(),
            SandboxError::Join(e) => e.into(),
            SandboxError::NoRoom(e) => e.into(),
            SandboxError::InUse(_) => refused(StatusCode::CONFLICT, "in_use", e),
        }
    }
}

/// A storage backend's failure, which can name what it runs on: logged, and answered without it.
fn storage_failed(e: anyhow::Error) -> Refused {
    let _ = writeln!(io::stderr(), "the storage backend failed: {e:#}");
    internal("the server's storage failed")
}

impl From<Refusal> for Refused {
    fn from(e: Refusal) -> Refused {
        match e {
            Refusal::InUse(_) => refused(StatusCode::CONFLICT, "in_use", e),
            Refusal::Exists(_) => refused(StatusCode::CONFLICT, "exists", e),
            Refusal::Unknown(_) => not_found(e),
            Refusal::Failed(e) => storage_failed(e),
        }
    }
}

/// No room for a machine, as the embedder's admission says: the protocol's `503` `capacity`,
/// nothing having been made.
impl From<NoRoom> for Refused {
    fn from(e: NoRoom) -> Refused {
        refused(StatusCode::SERVICE_UNAVAILABLE, "capacity", e)
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
    fn a_refusal_s_status_is_the_one_it_is_answered_with() {
        let refusals: [Refused; 4] = [
            refused(StatusCode::CONFLICT, "in_use", "in use").after(5),
            refusal(StatusCode::FORBIDDEN),
            refusal(StatusCode::BAD_GATEWAY),
            SandboxError::NotFound("no sandbox".into()).into(),
        ];
        let statuses = refusals.map(|refused| {
            let status = refused.status();
            (status, refused.into_response().status())
        });
        assert_eq!(
            statuses.map(|(status, _)| status),
            [
                StatusCode::CONFLICT,
                StatusCode::FORBIDDEN,
                StatusCode::INTERNAL_SERVER_ERROR,
                StatusCode::NOT_FOUND,
            ]
        );
        for (status, answered) in statuses {
            assert_eq!(status, answered);
        }
    }

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
            let internal: [Refused; 6] = [
                ImageStoreError::Fstree(tree()).into(),
                MachineError::Fstree(tree()).into(),
                MachineError::Io {
                    context: CONTEXT.into(),
                    source: source(),
                }
                .into(),
                MachineError::Volume(VolumeError::Io {
                    context: CONTEXT.into(),
                    source: source(),
                })
                .into(),
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

    #[tokio::test]
    async fn a_machine_that_did_not_start_is_answered_as_it_tells_it() {
        let why = "the machine's process ended with exit status: 1 before it was up";
        let told = format!("{why}; last logged:\nattaching the root's image: boot error");
        let refusal: Refused = MachineError::DidNotStart {
            account: format!(
                "{why}; last logged:\nvirtx_uvm::machine::machine: attaching the root's image: \
                 boot error: /home/virtx/project/a/images/rootfs/sha256/a.vmdk"
            ),
            told: told.clone(),
        }
        .into();
        let response = refusal.into_response();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["error"]["code"], "boot_failed");
        assert_eq!(body["error"]["message"], told.as_str());
    }

    #[tokio::test]
    async fn a_failed_image_job_says_why_without_the_server_s_paths() {
        const CONTEXT: &str = "opening /home/virtx/project/a/images/rootfs/sha256/a.vmdk";
        let source = || io::Error::new(io::ErrorKind::PermissionDenied, "under /home/virtx");
        let store = move || ImageStoreError::Io {
            context: CONTEXT.into(),
            source: source(),
        };
        let tree = move || FstreeError::Io {
            context: CONTEXT.into(),
            source: source(),
        };
        let on_files: [Failure; 6] = [
            Box::new(BuildError::Io {
                context: CONTEXT.into(),
                source: source(),
            }),
            Box::new(BuildError::Store(store())),
            Box::new(BuildError::Fstree(tree())),
            Box::new(RegistryError::Store(store())),
            Box::new(RegistryError::Fstree(tree())),
            Box::new(store()),
        ];
        for e in on_files {
            assert_eq!(failed(e), "the server's files: permission denied");
        }
        let cancelled = tokio::spawn(std::future::pending::<()>());
        cancelled.abort();
        let elsewhere: [Failure; 2] = [
            Box::new(BuildError::Join(cancelled.await.unwrap_err())),
            Box::new(RegistryError::Io {
                context: "making a registry client".into(),
                source: io::Error::other("no proxy"),
            }),
        ];
        for e in elsewhere {
            let said = e.to_string();
            assert_eq!(failed(e), said);
        }
        let why = "the machine's process ended with exit status: 1 before it was up";
        let machine = BuildError::Machine(MachineError::DidNotStart {
            account: format!("{why}; last logged:\n{CONTEXT}"),
            told: why.into(),
        });
        assert_eq!(failed(Box::new(machine)), why);
        let base = "base alpine:3.20: no image here is tagged alpine:3.20";
        assert_eq!(failed(base.into()), base);

        let root = std::env::temp_dir().join(format!("virtx-tcp-image-job-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let images = ImageStore::new(root.join("images")).unwrap();
        let registry = Arc::new(RegistryServer::new(images.clone(), "/images").unwrap());
        let home = Arc::new(Home::new(images, registry, &root).unwrap());
        let started = image_job(
            &home,
            ImageJobKind::Build,
            "app".into(),
            None,
            move |_| async move { Err(Failure::from(store())) },
        );
        assert!(started.is_ok());
        let job = Arc::clone(&home.image_jobs.lock().unwrap().values().next().unwrap().0);
        let ended = async {
            loop {
                if let ImageJob {
                    running: false,
                    error,
                    ..
                } = &*job.job.lock().unwrap()
                {
                    break error.clone();
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        let error = tokio::time::timeout(Duration::from_secs(10), ended)
            .await
            .unwrap();
        assert_eq!(
            error.as_deref(),
            Some("the server's files: permission denied")
        );
        let _ = std::fs::remove_dir_all(&root);
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

    /// A write the machine holds no room for is turned away for now, as `capacity`, with a
    /// `Retry-After`: not `ended`, which would say the stdin takes nothing more.
    #[tokio::test]
    async fn a_write_with_no_room_for_it_is_turned_away_for_now() {
        let response = Refused::from(SandboxError::StdinFull).into_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()[header::RETRY_AFTER], "1");
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["error"]["code"], "capacity");
        assert_eq!(body["error"].as_object().unwrap().len(), 2, "{body}");
    }
}
