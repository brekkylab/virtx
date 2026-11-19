//! The endpoints `virtx-core`'s `PROTOCOL.md` names, but the images' registry: those in a project
//! ([`router`]), each answered from the [`Home`] of the project its request reaches, which
//! [`access`](crate::access) puts in its extensions; and those of the projects themselves
//! ([`projects`]), answered from the [`Access`] there.
//!
//! Beside the protocol, what virtx cloud's node needs of a home ([`Home`]'s `pub` methods): its
//! storage kept past this server ([`Keeping`]), its
//! sandboxes themselves -- to count, park, move to another node or copy the disk of
//! ([`Home::sandboxes`]) -- a sandbox resized or its disk grown as it next boots
//! (`PUT /sandboxes/{id}/resources`, `/disk`), and the volumes listed a page at a time,
//! `?limit=&after=` -- the cloud's extension, which a client that sends neither does not see.
//!
//! A home's sandboxes are [`virtx_uvm::Sandboxes`], their disks snapshots of its store named
//! by their ids; what the home adds to them -- its keeping, its ports' capabilities -- it plugs
//! in as their [`Hooks`]. Room for their machines is `virtx-uvm`'s
//! ([`virtx_uvm::set_admission`]).

use std::collections::HashMap;
use std::ffi::OsString;
use std::fmt::Display;
use std::future::Future;
use std::hash::{BuildHasher as _, Hasher as _, RandomState};
use std::io::{self, Write as _};
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Extension, FromRequest, FromRequestParts, Request};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post, put};
use futures_util::future::BoxFuture;
use http::{HeaderMap, StatusCode, header};
use http_body_util::BodyExt as _;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::net::TcpStream;
use tokio::task::AbortHandle;

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use virtx_core::{
    DEFAULT_PROJECT, ImageBuildArgs, ImageJob, ImageJobEnd, ImageJobEntry, ImageJobKind,
    ImagePruneOutput, ImagePullArgs, ImagePushArgs, Log, PortProtocol, PortSpec, ProjectCreateArgs,
    ProjectListOutputItem, ProjectMemberListOutputItem, ProjectMemberSetArgs, Role,
    SandboxCreateArgs, SandboxExecArgs, SandboxSignalArgs, SandboxStdinArgs, Step,
    VolumeCreateArgs, VolumeListOutputItem, VolumeMountCreateArgs, VolumeMountListOutputItem,
    described_type, from_body,
};
use virtx_uvm::backend::{Backing, Refusal as BackendRefusal, Stored, Wanted as BackendWanted};
use virtx_uvm::sandbox::{Activity, Held, Hooks, Sandbox, Wanted};
use virtx_uvm::{
    DEFAULT_MEMORY_MIB, DEFAULT_VCPUS, Digest, Error as FstreeError, Files, ImageConfiguration,
    ImageHold, ImageStore, ImageStoreError, LOG_LIMIT, MachineError, MachineSpec, Network, NoRoom,
    Os, PublicImages, PublicRegistries, Reference, RegistryClient, RegistryError, RegistryProxy,
    RegistryServer, SandboxError, Sandboxes as SandboxStore, SnapshotError, SnapshotStore,
    StorageBackend, VolumeError, VolumeStore, build_step, signal_number,
};

use virtx_uvm::ProjectError;

use crate::access::{Access, Principal};
use crate::project::Project;

pub use virtx_uvm::sandbox::cloned;

/// A registry client for this server's pulls and pushes: anonymous pulls of Docker Hub's images
/// from its mirror at `mirror` first, given one, and shared through `public`, given one;
/// through `registries`.
fn registry(
    images: ImageStore,
    auth: Option<virtx_core::RegistryAuth>,
    mirror: Option<&str>,
    public: Option<Arc<dyn PublicImages>>,
    registries: &dyn RegistryProxy,
) -> Result<RegistryClient, RegistryError> {
    RegistryClient::new(images, auth)
        .with_mirror(mirror)
        .with_public(public)
        .through(Some(registries))
}

/// How old what nothing needs has to be before a prune takes it: a pull, push or build in
/// flight has put it there and not yet pointed a reference at it.
pub const PRUNE_GRACE: Duration = Duration::from_secs(60 * 60);

/// How long an image job that has ended is kept, for its end to be read, before it is removed.
const IMAGE_JOB_KEPT: Duration = Duration::from_secs(60);

/// An image job, with the entries its log carries.
type ImageJobOf = Job<ImageJob, ImageJobEntry>;

/// What one project's requests reach: the stores, and the sandboxes and jobs its members made
/// from them, which are theirs together. A handle: cloned, it is the same home.
#[derive(Clone)]
pub struct Home(Arc<HomeInner>);

impl Deref for Home {
    type Target = HomeInner;

    fn deref(&self) -> &HomeInner {
        &self.0
    }
}

/// What a [`Home`] holds.
pub struct HomeInner {
    images: ImageStore,
    /// Its images as an OCI registry, at `/images/oci/v2/`, for a tenant's home: its own.
    registry: Arc<RegistryServer>,
    pub snapshots: SnapshotStore,
    pub volumes: VolumeStore,
    /// The home: its sandboxes' records and disks are under its `sandbox/`.
    /// What its image jobs -- a pull, a push, a build's base -- connect to registries through, as
    /// the [`Access`] it is made for says ([`Home::through`]); [`PublicRegistries`] with none
    /// named until it does.
    registries: OnceLock<Arc<dyn RegistryProxy>>,
    /// Docker Hub's mirror, which its anonymous pulls of Docker Hub's images go to first, as the
    /// [`Access`] it is made for says ([`Home::through`]).
    mirror: OnceLock<Arc<str>>,
    /// Where its anonymous pulls are shared with other homes', if anywhere ([`Home::sharing`]).
    public: OnceLock<Arc<dyn PublicImages>>,
    pub root: PathBuf,
    /// The files of the volumes, over WebDAV, and of the sandboxes, through their machines.
    files: Files,
    /// This binary, as each machine is spawned.
    vm: Vec<OsString>,
    /// Every sandbox, its machine up or not, until it is deleted.
    sandboxes: SandboxStore,
    /// The image jobs, by id, and what cancels each, until each is removed or a while after
    /// it ends.
    image_jobs: Mutex<HashMap<String, (Arc<ImageJobOf>, AbortHandle)>>,
    /// Set once its project is to be removed: no sandbox or image job is added after.
    closed: AtomicBool,
    /// Where its project's data is kept beyond this machine, once it is given one
    /// ([`Home::backed_by`]): its sandboxes and its files take it from here.
    backing: Backing,
}

impl Home {
    /// The images in `images` -- its own, or shared with other projects -- and the snapshots,
    /// volumes and sandboxes under `root`, kept beyond this machine by a backend once it is
    /// given one ([`backed_by`](Home::backed_by)). Its image jobs connect to registries on public
    /// addresses alone, through [`PublicRegistries`] with none named, unless the [`Access`] it
    /// is made for says otherwise ([`Access::through`]).
    pub fn new(images: ImageStore, root: &Path) -> anyhow::Result<Home> {
        let snapshots = SnapshotStore::new(root)?;
        let volumes = VolumeStore::new(root)?;
        // The disks no sandbox was made of gone before anything counts what is here.
        virtx_uvm::sandbox::forget_unrecorded(&snapshots, root);
        let registry = RegistryServer::new(images.clone(), crate::access::REGISTRY_PATH)?;
        // Shared by the registry, the files, the sandboxes and the home.
        let backing = registry.backing().clone();
        let vm: Vec<OsString> = vec![
            std::env::current_exe()?.into(),
            crate::serve::MACHINE_SUBCOMMAND.into(),
        ];
        let hooks = Arc::new(HomeHooks {
            backing: backing.clone(),
            home: OnceLock::new(),
        });
        let sandboxes = SandboxStore::new(
            images.clone(),
            snapshots.clone(),
            volumes.clone(),
            root,
            vm.clone(),
            Arc::clone(&hooks) as Arc<dyn Hooks>,
            virtx_uvm::park_after(),
        )?;
        let home = Home(Arc::new(HomeInner {
            registry: Arc::new(registry),
            images,
            registries: OnceLock::new(),
            mirror: OnceLock::new(),
            public: OnceLock::new(),
            files: Files::new(volumes.clone()).backed_by(backing.clone()),
            snapshots,
            root: root.to_owned(),
            volumes,
            vm,
            sandboxes,
            image_jobs: Mutex::default(),
            closed: AtomicBool::default(),
            backing,
        }));
        let _ = hooks.home.set(Arc::downgrade(&home.0));
        Ok(home)
    }

    /// Park every sandbox, in a thread of its own: what they wrote is on their disks once each
    /// machine has ended, and each is kept, parked, for a server started again.
    pub fn close_all(&self) {
        self.sandboxes.close_all();
    }

    /// As [`close_all`](Self::close_all), blocking until each machine has stopped: not on an
    /// async runtime's thread.
    pub fn close_all_blocking(&self) {
        self.sandboxes.close_all_blocking();
    }

    /// Where the project's anonymous pulls are shared, as it was made to share them
    /// ([`Home::sharing`]).
    fn public(&self) -> Option<Arc<dyn PublicImages>> {
        self.public.get().cloned()
    }

    /// Its images, as an OCI registry.
    pub fn registry(&self) -> Arc<RegistryServer> {
        Arc::clone(&self.registry)
    }

    /// This, its project's data kept beyond this machine by `backend` as well, its own stores
    /// the backend's cache from now on: each of its sandboxes, those known again as it was made
    /// among them, takes its disk and volumes from the backend as its machine boots, its
    /// volumes and their files are made, listed, read, written and removed through it, and its
    /// registry lists the tags kept there and brings a manifest it is asked for here. Said
    /// once, as it is made, before any request reaches it. A home given none -- a server on its
    /// own -- keeps nothing beyond its stores. See [`StorageBackend`].
    pub fn backed_by(self, backend: Arc<dyn StorageBackend>) -> Home {
        self.backing.set(backend);
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
        if self.closed.load(Ordering::SeqCst) {
            return Err(forbidden("this project is being removed"));
        }
        let sandbox = self.sandboxes.take_in(id).await?;
        if let Some(backend) = self.backing.get() {
            for volume in &sandbox.args().volumes {
                backend.bring_up(Stored::Volume(&volume.volume)).await?;
            }
        }
        Ok(sandbox.answer(true))
    }

    /// Cloud only: a sandbox made of `args` under the id `id`, its ports behind `keys` (port
    /// by key), as one made elsewhere was -- what its client holds naming it here too -- over
    /// the disk here under that id if there is one, else a new one; booted unless not `booted`.
    /// For one kept nowhere to [take in](Home::take_in) from: made elsewhere and lost before it
    /// was kept.
    pub async fn make_as(
        &self,
        args: SandboxCreateArgs,
        id: String,
        keys: HashMap<String, u16>,
        booted: bool,
    ) -> Result<virtx_core::Sandbox, Refused> {
        let given = virtx_uvm::sandbox::Given { id, keys };
        Ok(self.sandboxes.create(args, Some(given), booted).await?)
    }

    /// Let go of sandbox `id` here, as this server lets go of each as it ends: its machine
    /// stopped if it is up, as parking stops it, and its record and disk left under this home's
    /// root, for a server to [take in](Home::take_in) -- another one it is moved to, this one
    /// again, or a backup's. Nothing of it is removed from the backend, where the home has one.
    /// From then on no request here reaches it. Refused `409` `in_use` while it is in use -- a
    /// job runs in it, or something reaches it -- and `404` for an id no sandbox here has. One
    /// kept here and not known again is let go of as it is. Blocks until its machine has
    /// stopped.
    pub async fn let_go(&self, id: &str) -> Result<(), Refused> {
        Ok(self.sandboxes.let_go(id).await?)
    }

    /// This, its image jobs connecting to registries through `registries`, and its anonymous
    /// pulls of Docker Hub's images going to `mirror` first: said once, as it is made.
    pub(crate) fn through(
        self,
        registries: Arc<dyn RegistryProxy>,
        mirror: Option<Arc<str>>,
    ) -> Home {
        let _ = self.registries.set(registries);
        if let Some(mirror) = mirror {
            let _ = self.mirror.set(mirror);
        }
        self
    }

    /// This, its anonymous pulls -- a pull, a build's base -- shared through `public`: an image
    /// it has is fetched from there and not pulled, and one pulled is told it. None is nothing
    /// shared, as a home has by default. A [`Homes`](crate::access::Homes) that makes homes for
    /// many projects shares public images between them so. See
    /// [`RegistryClient::with_public`].
    pub fn sharing(self, public: Option<Arc<dyn PublicImages>>) -> Home {
        if let Some(public) = public {
            let _ = self.public.set(public);
        }
        self
    }

    /// Docker Hub's mirror its anonymous pulls go to first, as its access says.
    fn mirror(&self) -> Option<Arc<str>> {
        self.mirror.get().cloned()
    }

    /// What its image jobs connect to registries through.
    fn registries(&self) -> Arc<dyn RegistryProxy> {
        match self.registries.get() {
            Some(registries) => Arc::clone(registries),
            None => Arc::new(PublicRegistries::default()),
        }
    }

    /// Its images.
    pub fn images(&self) -> &ImageStore {
        &self.images
    }

    /// The bytes its snapshots, volumes and images take here.
    pub async fn storage(&self) -> Option<Storage> {
        let (snapshots, volumes, images) = (
            self.snapshots.clone(),
            self.volumes.clone(),
            self.images.clone(),
        );
        tokio::task::spawn_blocking(move || {
            let snapshots = snapshots
                .list()
                .ok()?
                .iter()
                .map(|s| s.size().unwrap_or(0))
                .sum();
            let volumes = volumes
                .list()
                .ok()?
                .iter()
                .filter_map(|name| volumes.open(name).ok())
                .map(|v| v.size())
                .sum();
            Some(Storage {
                snapshots,
                volumes,
                images: size_of(images.root()),
            })
        })
        .await
        .ok()?
    }

    /// Volume `name` made here just now: kept beyond this machine too, where the home is --
    /// refused, and removed here, where one of its name is kept already.
    async fn made_volume(&self, name: &str) -> Result<(), Refused> {
        let Some(backend) = self.backing.get() else {
            return Ok(());
        };
        if let Err(e) = backend.made(Stored::Volume(name), 0).await {
            let _ = self.volumes.remove(name);
            return Err(e.into());
        }
        Ok(())
    }

    /// Delete each sandbox unused for [`reclaim_after`](virtx_uvm::reclaim_after), if that is
    /// ever ([`SandboxStore::reclaim`]).
    pub fn reclaim(&self) {
        if let Some(after) = virtx_uvm::reclaim_after() {
            self.sandboxes.reclaim(after);
        }
    }

    /// Its sandboxes, whether each one's machine is up or not: for a server's own routes
    /// ([`serve::router`](crate::serve::router)'s `own`) and its work between requests, which
    /// may find, count or park them.
    pub fn sandboxes(&self) -> Sandboxes<'_> {
        Sandboxes(self)
    }

    /// Park every sandbox here that nothing uses now ([`Sandbox::park`]), however recently it
    /// was used, side by side; those in use are left as they are.
    pub async fn park_unused(&self) {
        self.sandboxes.park_unused().await;
    }

    /// Park every sandbox here, in use or not ([`Sandbox::park_anyway`]), side by side: what
    /// runs in each ends, as cancelling it does, and each is kept, to boot again when it is next
    /// used.
    pub async fn park_all(&self) {
        self.sandboxes.park_all().await;
    }

    /// A tunnel to the port `capability` opens, in whichever of this home's sandboxes it is
    /// one of, its machine up or not: a connection through it to a parked sandbox's port is
    /// refused, and does not boot it.
    pub fn tunnel(&self, capability: &str) -> Option<Tunnel> {
        let (sandbox, port) = self.sandboxes.find_port(capability)?;
        Some(Tunnel {
            activity: sandbox.activity(),
            port,
        })
    }

    /// Close it to every sandbox and image job from now on, for its project to be removed:
    /// `false`, and left open, while a sandbox lives here -- its machine up or not, of this
    /// process or another.
    pub fn close(&self) -> bool {
        if !self.sandboxes.list(None).is_empty() || virtx_uvm::Sandbox::lives_under(&self.root) {
            return false;
        }
        self.closed.store(true, Ordering::SeqCst);
        true
    }

    /// Whether a sandbox whose machine is up here has the disk `name`, and so holds it.
    pub fn holds(&self, name: &str) -> bool {
        self.sandboxes.running_on(name).is_some()
    }

    /// The sandbox `id`, its machine up or not.
    fn sandbox(&self, id: &str) -> Result<Arc<Sandbox>, Refused> {
        Ok(self.sandboxes.get(id)?)
    }
}

/// A home's sandboxes, as each is when it is asked: those the home knows, each whether its
/// machine is up or not ([`Home::sandboxes`]). One kept in the home and not known again, which
/// takes only its `DELETE`, is none of them.
pub struct Sandboxes<'a>(&'a Home);

impl Sandboxes<'_> {
    /// Each of them, in no set order.
    pub fn list(&self) -> Vec<Arc<Sandbox>> {
        let store = &self.0.sandboxes;
        let listed = store.list(None).into_iter();
        listed
            .filter_map(|answer| store.get(&answer.id).ok())
            .collect()
    }

    /// The sandbox `id`, if it is one of them.
    pub fn get(&self, id: &str) -> Option<Arc<Sandbox>> {
        self.0.sandboxes.get(id).ok()
    }

    /// How many there are, and how many of those have their machine up now.
    pub fn counts(&self) -> (usize, usize) {
        let all = self.list();
        let up = all.iter().filter(|sandbox| sandbox.is_up()).count();
        (all.len(), up)
    }

    /// The sandbox `id`, if it is one of them and its machine is up now: what writes its disk.
    pub fn running_on(&self, id: &str) -> Option<Arc<Sandbox>> {
        self.0.sandboxes.running_on(id)
    }
}

/// A connection through one of a sandbox's ports, counted until it drops: a sandbox with one
/// is in use, whatever its jobs do.
pub struct Tunnel {
    activity: Activity,
    port: u16,
}

impl Tunnel {
    /// The port, in the sandbox's machine while it is up.
    pub async fn connect(&self) -> io::Result<TcpStream> {
        self.activity.sandbox().connect(self.port).await
    }

    /// Ended as nothing done, where its sandbox's machine is not up: not a use of it
    /// ([`Activity::unused`]).
    pub fn refused(self) {
        if !self.activity.sandbox().is_up() {
            self.activity.unused();
        }
    }
}

/// What a home plugs into its sandboxes: their disks and volumes kept beyond this machine by its
/// backend, once it has one ([`Home::backed_by`]), and their ports named by capabilities.
struct HomeHooks {
    backing: Backing,
    /// The home, once it is made.
    home: OnceLock<Weak<HomeInner>>,
}

impl HomeHooks {
    fn backend(&self) -> Option<&Arc<dyn StorageBackend>> {
        self.backing.get()
    }
}

/// What a home refused, as its sandboxes refuse it.
fn hooked(e: impl Into<Refused>) -> SandboxError {
    let Refused(status, code, message) = e.into();
    SandboxError::Refused {
        status: status.as_u16(),
        code,
        message,
    }
}

impl Hooks for HomeHooks {
    fn want<'a>(&'a self, wanted: Wanted<'a>) -> BoxFuture<'a, Result<(), SandboxError>> {
        Box::pin(async move {
            let Some(backend) = self.backend() else {
                return Ok(());
            };
            let fetched = match wanted {
                Wanted::Tag(tag) => match tag.parse::<Reference>() {
                    Ok(tag) => backend.want(BackendWanted::Tag(&tag)).await,
                    // Refused as the store refuses it, once it is looked for here.
                    Err(_) => Ok(()),
                },
                Wanted::Image(digest) => backend.want(BackendWanted::Image(digest)).await,
            };
            fetched.map_err(|e| hooked(internal(format!("{e:#}"))))
        })
    }

    /// Each port behind a capability of its own, which only the answers that open it carry:
    /// who has the URL reaches the port, and nothing else. Relative, to the URL the request
    /// came to, since what reaches this server from outside is not this server's to know.
    fn port(&self, spec: &PortSpec, kept: Option<String>) -> (Option<String>, String) {
        let capability = kept.unwrap_or_else(|| capability_for(spec.protocol));
        let url = format!("ports/{capability}");
        (Some(capability), url)
    }

    /// Kept beyond this machine from now on, where the home is: refused where one of its id is
    /// kept already, and the store removes what it made.
    fn made<'a>(&'a self, id: &'a str, size: u64) -> BoxFuture<'a, Result<(), SandboxError>> {
        Box::pin(async move {
            match self.backend() {
                Some(backend) => backend.made(Stored::Disk(id), size).await.map_err(hooked),
                None => Ok(()),
            }
        })
    }

    /// Its disk taken where it is kept, and each volume it writes; each it only reads brought
    /// up: what is held while its machine runs. Refused while another server holds either.
    fn booting<'a>(
        &'a self,
        sandbox: &'a Sandbox,
        _spec: &'a MachineSpec,
    ) -> BoxFuture<'a, Result<Vec<Held>, SandboxError>> {
        Box::pin(async move {
            let Some(backend) = self.backend() else {
                return Ok(Vec::new());
            };
            let mut held = vec![
                backend
                    .take(Stored::Disk(sandbox.id()))
                    .await
                    .map_err(hooked)?,
            ];
            for asked in &sandbox.args().volumes {
                let volume = Stored::Volume(&asked.volume);
                match asked.readonly {
                    true => backend.bring_up(volume).await.map_err(hooked)?,
                    false => held.push(backend.take(volume).await.map_err(hooked)?),
                }
            }
            Ok(held)
        })
    }

    fn booted(&self, id: &str, took: Duration, lazily: bool) {
        eprintln!(
            "booted {id} in {} ms{}",
            took.as_millis(),
            if lazily { ", lazily" } else { "" }
        );
    }

    fn said(&self, id: &str, level: &str, line: &str) {
        machine_said(id, level, line);
    }

    fn materialize<'a>(
        &'a self,
        id: &'a str,
        disk: &'a Path,
    ) -> BoxFuture<'a, Result<(), SandboxError>> {
        Box::pin(async move {
            match self.backend() {
                Some(backend) => backend
                    .materialize(Stored::Disk(id), disk)
                    .await
                    .map_err(hooked),
                None => Ok(()),
            }
        })
    }

    /// Its disk removed from where it is kept first, where the home is -- one never kept there,
    /// made and deleted before it was, is nothing to remove.
    fn deleting<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<(), SandboxError>> {
        Box::pin(async move {
            match self
                .backend()
                .map(|backend| backend.remove(Stored::Disk(id)))
            {
                Some(removing) => match removing.await {
                    Ok(()) | Err(BackendRefusal::Unknown(_)) => Ok(()),
                    Err(e) => Err(hooked(e)),
                },
                None => Ok(()),
            }
        })
    }
}

/// A capability for a port reached as `protocol`: an `http` one's is a host name's first label
/// as a gateway forms its URL -- 32 hex digits, of the 63 a label may have -- and a `tcp` one's
/// twice as long.
fn capability_for(protocol: PortProtocol) -> String {
    match protocol {
        PortProtocol::Http => uuid::Uuid::new_v4().simple().to_string(),
        PortProtocol::Tcp => format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        ),
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
        .route("/sandboxes/", get(sandbox_list).post(sandbox_create))
        .route("/sandboxes/{id}", get(sandbox_get).delete(sandbox_remove))
        .route("/sandboxes/{id}/disk", put(sandbox_disk))
        .route("/sandboxes/{id}/resources", put(sandbox_resources))
        .route("/sandboxes/{id}/files", any(sandbox_files))
        .route("/sandboxes/{id}/files/", any(sandbox_files))
        .route("/sandboxes/{id}/files/{*path}", any(sandbox_files))
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
    let (images, public, registries) = (home.images.clone(), home.public(), home.registries());
    let mirror = home.mirror();
    image_job(
        &home,
        ImageJobKind::Pull,
        args.reference,
        move |report| async move {
            let registry = registry(
                images.clone(),
                args.auth,
                mirror.as_deref(),
                public,
                &*registries,
            )?;
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
    let (images, registries) = (home.images.clone(), home.registries());
    image_job(
        &home,
        ImageJobKind::Push,
        args.reference,
        move |report| async move {
            let registry = registry(images, args.auth, None, None, &*registries)?;
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
    network: Network::Public,
};

async fn image_build(Extension(home): Extension<Arc<Home>>, Whole(request): Whole) -> Answer {
    let args: ImageBuildArgs = body(&request)?;
    let reference = reference(&args.reference)?;
    if args.recipe.base != "scratch" {
        self::reference(&args.recipe.base)?;
    }
    let (images, vm, public) = (home.images.clone(), home.vm.clone(), home.public());
    let registries = home.registries();
    let mirror = home.mirror();
    let recipe = args.recipe;
    // Room for one machine as the build is asked for, if a step of it boots one -- its steps
    // boot one at a time -- so a build there is none for is refused as a sandbox is.
    let boots = recipe.steps.iter().any(|step| matches!(step, Step::Run(_)));
    let room = match boots {
        true => virtx_uvm::admit(&BUILD_MACHINE)?,
        false => None,
    };
    image_job(
        &home,
        ImageJobKind::Build,
        args.reference,
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
                let before = layers.len();
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
                let layered = layers.len() > before;
                virtx_uvm::history_kept(&mut image, step.to_string(), layered, layers.len());
            }
            virtx_uvm::recipe_kept(&mut image, &recipe);
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
/// pulled into the store now, anonymously, through `registries`, from `mirror` first if it is
/// Docker Hub's and shared through `public`, and left untagged: a build tags only what it
/// builds.
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
            registry(images.clone(), None, mirror, public, registries)?
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

async fn volume_list(Extension(home): Extension<Arc<Home>>, Query(page): Query<Page>) -> Answer {
    if let Some(backend) = home.backing.get()
        && let Some(kept) = backend
            .volumes()
            .await
            .map_err(|e| internal(format!("{e:#}")))?
    {
        return Ok(json(&page.of(kept, |v| &v.name)));
    }
    let mut listed = Vec::new();
    for name in page.of(home.volumes.list()?, |n| n) {
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
    home.made_volume(&name).await?;
    Ok(created(format!("/volumes/{name}"), &volume(&home, name)?))
}

async fn volume_get(
    Extension(home): Extension<Arc<Home>>,
    Segments(name): Segments<String>,
) -> Answer {
    if let Some(backend) = home.backing.get()
        && let Some(kept) = backend
            .volumes()
            .await
            .map_err(|e| internal(format!("{e:#}")))?
    {
        let kept = kept.into_iter().find(|v| v.name == name);
        return Ok(json(&kept.ok_or(VolumeError::Unknown(name))?));
    }
    Ok(json(&volume(&home, name)?))
}

/// Remove it, and what is kept of it beyond this machine first: refused before anything is
/// removed while a sandbox here mounts it, or the backend refuses.
async fn volume_remove(
    Extension(home): Extension<Arc<Home>>,
    Segments(name): Segments<String>,
) -> Answer {
    if let Ok(volume) = home.volumes.open(&name) {
        // Mounted here: refused before anything is removed.
        drop(volume.hold_alone()?);
    }
    let kept = match home.backing.get() {
        Some(backend) => match backend.remove(Stored::Volume(&name)).await {
            Ok(()) => true,
            // Kept nowhere else: as the volume here says.
            Err(virtx_uvm::backend::Refusal::Unknown(_)) => false,
            Err(e) => return Err(e.into()),
        },
        None => false,
    };
    match home.volumes.remove(&name) {
        Ok(()) => {}
        Err(VolumeError::Unknown(_)) if kept => {}
        Err(e) => return Err(e.into()),
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
    // Held, and kept again after: its mounts go with it to the next node that takes it.
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DiskArgs {
    disk_gib: u32,
}

/// `PUT /sandboxes/{id}/disk`, `{"disk_gib": n}`: its disk grown to `n` GiB -- never shrunk:
/// smaller than it is, refused `400`. Grown as it next boots, what is on it kept
/// ([`virtx_uvm::Snapshot::grow`]). Answered `{disk_gib, up}`: `up`, its machine is up now, and
/// its disk is grown once it boots again.
async fn sandbox_disk(
    Extension(home): Extension<Arc<Home>>,
    Segments(id): Segments<String>,
    request: Bytes,
) -> Answer {
    let args: DiskArgs = body(&request)?;
    let up = home.sandbox(&id)?.grow_disk(args.disk_gib)?;
    Ok(json(&serde_json::json!({
        "disk_gib": args.disk_gib,
        "up": up,
    })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResourcesArgs {
    #[serde(default)]
    vcpus: Option<u8>,
    #[serde(default)]
    memory_mib: Option<u32>,
    #[serde(default)]
    disk_gib: Option<u32>,
}

/// `PUT /sandboxes/{id}/resources`, `{"vcpus", "memory_mib", "disk_gib"}`, each optional: its
/// machine resized, and its disk grown -- as it next boots. A machine is the size it booted as:
/// one up whose vCPUs or memory change is parked now if nothing uses it, and refused `409`,
/// `in_use`, if something does. Its disk never shrunk, refused `400`. Answered
/// `{vcpus, memory_mib, disk_gib, up}`, as it is to be, `up` whether its machine is up now.
async fn sandbox_resources(
    Extension(home): Extension<Arc<Home>>,
    Segments(id): Segments<String>,
    request: Bytes,
) -> Answer {
    let args: ResourcesArgs = body(&request)?;
    let sandbox = home.sandbox(&id)?;
    let resized = sandbox
        .resize(args.vcpus, args.memory_mib, args.disk_gib)
        .await?;
    Ok(json(&resized))
}

/// A request to the files of a sandbox, over WebDAV, asked of its machine, booted first if it is
/// parked. One whose machine has exited answers `409`, `ended`.
async fn sandbox_files(
    Extension(home): Extension<Arc<Home>>,
    Segments(segments): Segments<HashMap<String, String>>,
    request: axum::extract::Request,
) -> Answer {
    let id = &segments["id"];
    let sandbox = home.sandbox(id)?;
    // In use from here until the answer has been sent -- taken first, so that it is not parked
    // unused in between. Refused, it is not used: an exited sandbox is not kept longer for it.
    let activity = sandbox.activity();
    let machine = match sandbox.machine().await {
        Ok(machine) => machine,
        Err(e) => {
            activity.unused();
            return Err(e.into());
        }
    };
    Ok(home.files.sandbox(id, machine, activity, request).await)
}

#[derive(Deserialize)]
struct Named {
    name: Option<String>,
}

impl Params for Named {
    const TAKES: &[(&str, &str)] = &[("name", "a sandbox's name")];
}

/// Every sandbox; `?name=` those named so, one at most.
async fn sandbox_list(Extension(home): Extension<Arc<Home>>, Query(named): Query<Named>) -> Answer {
    Ok(json(&home.sandboxes.list(named.name.as_deref())))
}

async fn sandbox_create(Extension(home): Extension<Arc<Home>>, Whole(request): Whole) -> Answer {
    let args: SandboxCreateArgs = body(&request)?;
    if home.closed.load(Ordering::SeqCst) {
        return Err(forbidden("this project is being removed"));
    }
    let answer = home.sandboxes.create(args, None, true).await?;
    Ok(created(format!("/sandboxes/{}", answer.id), &answer))
}

async fn sandbox_get(
    Extension(home): Extension<Arc<Home>>,
    Segments(id): Segments<String>,
) -> Answer {
    Ok(json(&home.sandboxes.answer(&id)?))
}

/// Remove it, and its disk kept beyond this machine first: refused, and nothing removed, where
/// the backend refuses.
async fn sandbox_remove(
    Extension(home): Extension<Arc<Home>>,
    Segments(id): Segments<String>,
) -> Answer {
    home.sandboxes.delete(&id).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn sandbox_job_list(
    Extension(home): Extension<Arc<Home>>,
    Segments(id): Segments<String>,
) -> Answer {
    Ok(json(&home.sandbox(&id)?.jobs()))
}

/// `POST /sandboxes/{id}/jobs/exec/`, answered with the job -- or, to a request that accepts
/// `text/event-stream`, `201` with the job's log as the body, the job in [`JOB_HEADER`] and its
/// path in `Location`: its output and end with no second request.
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
    // A job started in a parked sandbox boots it first.
    let job = sandbox.exec(args.cmd, args.stdin, timeout).await?;
    let answered = job.get();
    let location = format!("/sandboxes/{id}/jobs/{}", answered.id);
    let streamed = headers
        .get(header::ACCEPT)
        .and_then(|accept| accept.to_str().ok())
        .is_some_and(|accept| accept.contains("text/event-stream"));
    let said = ascii(&serde_json::to_string(&answered).map_err(internal)?);
    // A job too long for a header is answered alone, and its log read with a request of its own.
    if !streamed || said.len() > JOB_HEADER_MOST {
        return Ok(created(location, &answered));
    }
    let mut log = events(job.log(), &HeaderMap::new());
    *log.status_mut() = StatusCode::CREATED;
    let value = |value: &str| header::HeaderValue::from_str(value).map_err(internal);
    log.headers_mut()
        .insert(header::LOCATION, value(&location)?);
    log.headers_mut().insert(JOB_HEADER, value(&said)?);
    Ok(log)
}

/// Where an exec answered with its log says its job.
const JOB_HEADER: &str = "virtx-job";

/// The longest job, as JSON, [`JOB_HEADER`] carries.
const JOB_HEADER_MOST: usize = 8 << 10;

/// `json` with every character past ASCII, and `DEL`, escaped, `\uXXXX` as JSON writes one:
/// the same JSON, and a header's value.
fn ascii(json: &str) -> String {
    let mut escaped = String::with_capacity(json.len());
    for c in json.chars() {
        if c.is_ascii() && c != '\u{7f}' {
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

/// Write to the stdin of a job started with one, and close it if `eof`: answered once it is
/// written, so a job that does not read holds the answer back. Refused `409`, `ended`, for one
/// started without, one whose stdin is closed, and one that has ended.
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

/// What a sandbox's machine logged: a warning or an error, or everything with
/// `VIRTX_MACHINE_LOG` set.
///
/// Written where it can be, and dropped where it can't: this is said on the thread that reads
/// the machine's reports, and `eprintln!` panics on a stderr nobody reads any more -- which would
/// end that thread, and with it every port the machine publishes.
fn machine_said(sandbox: &str, level: &str, line: &str) {
    use std::io::Write as _;
    static ALL: OnceLock<bool> = OnceLock::new();
    let all = *ALL.get_or_init(|| std::env::var_os("VIRTX_MACHINE_LOG").is_some());
    if all || matches!(level, "WARN" | "ERROR") {
        let _ = writeln!(io::stderr().lock(), "sandbox {sandbox}: {level}: {line}");
    }
}

/// What a project's snapshots, volumes and images take on this server, in bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Storage {
    pub snapshots: u64,
    pub volumes: u64,
    pub images: u64,
}

/// The bytes the files under `dir` take, links not followed.
fn size_of(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| match entry.file_type() {
            Ok(kind) if kind.is_dir() => size_of(&entry.path()),
            Ok(kind) if kind.is_file() => entry.metadata().map_or(0, |meta| meta.len()),
            _ => 0,
        })
        .sum()
}

/// A page of a listing, the cloud's extension: those after `after` by name, `limit` at most.
/// Neither, and the whole listing.
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Page {
    after: Option<String>,
    limit: Option<u32>,
}

impl Params for Page {
    const TAKES: &[(&str, &str)] = &[
        ("after", "a name"),
        ("limit", "a whole number up to 4294967295"),
    ];
}

impl Page {
    /// `items`, in order by name as `name` reads it, cut to this page.
    fn of<T>(&self, mut items: Vec<T>, name: impl Fn(&T) -> &str) -> Vec<T> {
        items.sort_by(|a, b| name(a).cmp(name(b)));
        let after = self.after.as_deref();
        let items = items
            .into_iter()
            .filter(|item| after.is_none_or(|after| name(item) > after));
        match self.limit {
            Some(limit) => items.take(limit as usize).collect(),
            None => items.collect(),
        }
    }
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

/// A query read as a `T`: refused `400` `invalid` when it does not read as one, naming the
/// parameter and what it takes ([`Params`]).
struct Query<T>(T);

/// The parameters a query takes, as a client is told them when it does not read.
trait Params {
    /// Each parameter, and what it takes: `("limit", "a whole number")`.
    const TAKES: &[(&str, &str)];
}

impl<T, S> FromRequestParts<S> for Query<T>
where
    T: DeserializeOwned + Params,
    S: Send + Sync,
{
    type Rejection = Refused;

    async fn from_request_parts(parts: &mut http::request::Parts, _: &S) -> Result<Self, Refused> {
        let query = parts.uri.query().unwrap_or_default();
        serde_urlencoded::from_str(query)
            .map(Query)
            .map_err(|_| invalid(unread_query::<T>(query)))
    }
}

/// Why `query` does not read as a `T`: the first parameter that does not read alone, or one
/// given twice.
fn unread_query<T: DeserializeOwned + Params>(query: &str) -> String {
    let Ok(pairs) = serde_urlencoded::from_str::<Vec<(String, String)>>(query) else {
        return "the query does not read".to_owned();
    };
    for (i, (key, value)) in pairs.iter().enumerate() {
        if pairs[..i].iter().any(|(earlier, _)| earlier == key) {
            return format!("{key}: given twice");
        }
        let alone = serde_urlencoded::to_string([(key, value)]).unwrap_or_default();
        if serde_urlencoded::from_str::<T>(&alone).is_ok() {
            continue;
        }
        return match T::TAKES.iter().find(|(param, _)| param == key) {
            Some((_, takes)) => format!("{key}: {takes}, not {value:?}"),
            None => {
                let params: Vec<_> = T::TAKES.iter().map(|(param, _)| *param).collect();
                format!(
                    "{key}: not a parameter; the parameters are {}",
                    params.join(", ")
                )
            }
        };
    }
    "the query does not read".to_owned()
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
pub struct Refused(StatusCode, &'static str, String);

pub fn refused(status: StatusCode, code: &'static str, message: impl Display) -> Refused {
    Refused(status, code, message.to_string())
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

impl Refused {
    /// What it is answered with.
    pub fn status(&self) -> StatusCode {
        self.0
    }
}

/// How long a client is told to wait before it asks again for what found no room.
const CAPACITY_RETRY_AFTER: &str = "5";

impl IntoResponse for Refused {
    fn into_response(self) -> Response {
        let Refused(status, code, message) = self;
        let body = serde_json::json!({"error": {"code": code, "message": message}});
        let mut response = (status, json(&body)).into_response();
        if code == "capacity" {
            let wait = http::HeaderValue::from_static(CAPACITY_RETRY_AFTER);
            response.headers_mut().insert(header::RETRY_AFTER, wait);
        }
        response
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

impl From<BackendRefusal> for Refused {
    fn from(e: BackendRefusal) -> Refused {
        match e {
            BackendRefusal::InUse(_) => refused(StatusCode::CONFLICT, "in_use", e),
            BackendRefusal::Exists(_) => refused(StatusCode::CONFLICT, "exists", e),
            BackendRefusal::Unknown(_) => not_found(e),
            BackendRefusal::Failed(e) => storage_failed(e),
        }
    }
}

/// What a storage backend failed with: said on the server's stderr, and answered `500` with no
/// word of why, which can name what the backend runs on.
fn storage_failed(e: anyhow::Error) -> Refused {
    let _ = writeln!(io::stderr(), "the storage backend failed: {e:#}");
    internal("the server's storage failed")
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

impl From<tokio::task::JoinError> for Refused {
    fn from(e: tokio::task::JoinError) -> Refused {
        internal(e)
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
            SandboxError::InUse(_) => refused(StatusCode::CONFLICT, "in_use", e),
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
            SandboxError::Backend(e) => e.into(),
            SandboxError::Machine(e) => e.into(),
            SandboxError::Join(e) => e.into(),
            SandboxError::NoRoom(e) => e.into(),
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
    fn a_page_is_those_after_a_name_in_order_up_to_its_limit() {
        let names = || vec!["c", "a", "d", "b"];
        let page = |after: Option<&str>, limit| Page {
            after: after.map(str::to_owned),
            limit,
        };
        assert_eq!(Page::default().of(names(), |n| n), ["a", "b", "c", "d"]);
        assert_eq!(page(None, Some(2)).of(names(), |n| n), ["a", "b"]);
        assert_eq!(page(Some("b"), Some(1)).of(names(), |n| n), ["c"]);
        assert_eq!(page(Some("d"), None).of(names(), |n| n), Vec::<&str>::new());
    }

    #[test]
    fn a_port_s_capability_is_hex_32_digits_for_http_and_64_for_tcp() {
        let hex = |c: &str| c.bytes().all(|b| b.is_ascii_hexdigit());
        let (http, tcp) = (
            capability_for(PortProtocol::Http),
            capability_for(PortProtocol::Tcp),
        );
        assert!(hex(&http) && http.len() == 32, "{http}");
        assert!(hex(&tcp) && tcp.len() == 64, "{tcp}");
    }
}

#[cfg(test)]
mod failures {
    use super::*;

    /// A job with a command past ASCII, and `DEL`, is said in its header as the same JSON.
    #[test]
    fn a_job_past_ascii_is_said_in_a_header_as_the_same_json() {
        let json = serde_json::json!({"cmd": ["echo", "héllo 😀", "a\u{7f}b"]}).to_string();
        let said = ascii(&json);
        assert!(header::HeaderValue::from_str(&said).is_ok(), "{said}");
        assert!(said.contains(r"h\u00e9llo \ud83d\ude00"), "{said}");
        let read: serde_json::Value = serde_json::from_str(&said).unwrap();
        assert_eq!(read.to_string(), json);
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
            let internal: [Refused; 2] = [
                ImageStoreError::Fstree(tree()).into(),
                ProjectError::Io {
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
}
