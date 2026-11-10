//! The endpoints `virtx-core`'s `PROTOCOL.md` names, but the images' registry, each answered from
//! the [`Home`] of the project its request reaches, which [`access`](crate::access) puts in its
//! extensions.
//!
//! Beside the protocol, what virtx cloud's node needs of a home ([`Home`]'s `pub` methods): its
//! storage kept past this server ([`Keeping`]), its machines held to the node's [`Capacity`],
//! a sandbox opened again under the id it had on another node ([`Home::open_as`]), a sandbox
//! resized or its disk grown as it next starts (`PUT /sandboxes/{id}/resources`, `/disk`), and
//! the volumes listed a page at a time, `?limit=&after=` -- the cloud's extension, which a
//! client that sends neither does not see.
//!
//! A home's sandboxes are [`virtx_uvm::Sandboxes`], their disks snapshots of its store named
//! by their ids; what the home adds to them -- its keeping, its capacity, its ports'
//! capabilities -- it plugs in as their [`Hooks`].

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fmt::Display;
use std::future::Future;
use std::hash::{BuildHasher as _, Hasher as _, RandomState};
use std::io;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Extension, Path as Segments, Query};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post, put};
use futures_util::future::BoxFuture;
use http::{HeaderMap, StatusCode, header};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::net::TcpStream;
use tokio::task::AbortHandle;

use virtx_core::{
    ImageBuildArgs, ImageCommitArgs, ImageJob, ImageJobEnd, ImageJobEntry, ImageJobKind,
    ImagePruneOutput, ImagePullArgs, ImagePushArgs, Log, PortProtocol, PortSpec, SandboxCreateArgs,
    SandboxExecArgs, SandboxPortOpenArgs, SandboxSignalArgs, SandboxStdinArgs, VolumeCreateArgs,
    VolumeListOutputItem, VolumeMountCreateArgs, VolumeMountListOutputItem,
};
use virtx_uvm::sandbox::{Activity, Given, Held, Hooks, Sandbox, Wanted, Written};
use virtx_uvm::{
    DEFAULT_MEMORY_MIB, DEFAULT_VCPUS, Digest, Files, ImageConfiguration, ImageHold, ImageStore,
    ImageStoreError, LOG_LIMIT, MachineError, MachineSpec, Network, Os, Reference, RegistryClient,
    RegistryServer, SandboxError, Sandboxes, SnapshotError, SnapshotStore, VolumeError,
    VolumeStore, build_step, signal_number,
};

use crate::keeping::{Keeping, Kind, MakeKeeping, Refusal};

pub use virtx_uvm::sandbox::{AUTO_DELETE_DAYS, AUTO_STOP_MINUTES, Terminal, cloned};

/// What this server lets its machines take at once, every project's together: past it, a
/// sandbox is refused with `capacity` rather than boot a machine the host has no room for.
/// Memory is committed only as a guest touches it, so the host's is overcommitted twice;
/// vCPUs four times, since most sandboxes wait more than they compute.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capacity {
    pub vcpus: u64,
    pub memory_mib: u64,
}

impl Capacity {
    /// Four vCPUs for each of the host's, and twice its memory.
    pub fn of_host() -> Capacity {
        let cpus = std::thread::available_parallelism().map_or(1, |n| n.get()) as u64;
        Capacity {
            vcpus: cpus * 4,
            memory_mib: host_memory_mib() * 2,
        }
    }
}

/// The host's memory, in MiB.
#[cfg(unix)]
fn host_memory_mib() -> u64 {
    // SAFETY: sysconf reads a constant of the system's, and has no other effect.
    let (pages, page) = unsafe {
        (
            libc::sysconf(libc::_SC_PHYS_PAGES),
            libc::sysconf(libc::_SC_PAGESIZE),
        )
    };
    (pages.max(0) as u64 * page.max(0) as u64) >> 20
}

/// The host's memory, in MiB, where it is not asked for: as much as a machine is by default,
/// four times over.
#[cfg(not(unix))]
fn host_memory_mib() -> u64 {
    4 * u64::from(DEFAULT_MEMORY_MIB)
}

static HELD_TO: OnceLock<Capacity> = OnceLock::new();
/// The vCPUs and memory the machines up now take.
static TAKEN: Mutex<(u64, u64)> = Mutex::new((0, 0));

/// What this server's machines are held to, and what those up take now: what the gateway
/// places sandboxes by.
pub fn capacity_and_taken() -> (Capacity, Capacity) {
    let capacity = *HELD_TO.get_or_init(Capacity::of_host);
    let (vcpus, memory_mib) = *TAKEN.lock().unwrap();
    (capacity, Capacity { vcpus, memory_mib })
}

/// Hold this server's machines to `capacity`, from now on; [`Capacity::of_host`] without.
pub fn set_capacity(capacity: Capacity) {
    let _ = HELD_TO.set(capacity);
}

/// Docker Hub's mirror, which this server's anonymous pulls of its images go to first, as an
/// embedder sets it (virtx cloud's nodes do): none, unless set. See
/// [`RegistryClient::with_mirror`].
static REGISTRY_MIRROR: OnceLock<String> = OnceLock::new();

/// Pull Docker Hub's images from its mirror at `mirror` first, from now on.
pub fn set_registry_mirror(mirror: &str) {
    let _ = REGISTRY_MIRROR.set(mirror.to_owned());
}

/// A registry client for this server's pulls: anonymous ones from Docker Hub's mirror, if one
/// was set.
fn registry(images: ImageStore, auth: Option<virtx_core::RegistryAuth>) -> RegistryClient {
    RegistryClient::new(images, auth).with_mirror(REGISTRY_MIRROR.get().map(String::as_str))
}

/// A minute made `minute` long, as a test waiting on sandboxes stopping by themselves makes it;
/// before any sandbox is tended. See [`virtx_uvm::sandbox::set_minute`].
pub fn set_minute(minute: Duration) {
    virtx_uvm::sandbox::set_minute(minute);
}

pub(crate) fn minute() -> Duration {
    virtx_uvm::sandbox::minute()
}

/// What a machine up takes of the capacity, until this drops.
struct Taken(u64, u64);

impl Taken {
    fn of(spec: &MachineSpec) -> Result<Taken, Refused> {
        let capacity = *HELD_TO.get_or_init(Capacity::of_host);
        let (vcpus, memory_mib) = (u64::from(spec.vcpus), u64::from(spec.ram_mib));
        let mut taken = TAKEN.lock().unwrap();
        if taken.0 + vcpus > capacity.vcpus || taken.1 + memory_mib > capacity.memory_mib {
            return Err(refused(
                StatusCode::SERVICE_UNAVAILABLE,
                "capacity",
                format!(
                    "this server has no room for a machine of {vcpus} vCPUs and {memory_mib} \
                     MiB now: its machines take {} of {} vCPUs and {} of {} MiB",
                    taken.0, capacity.vcpus, taken.1, capacity.memory_mib
                ),
            ));
        }
        taken.0 += vcpus;
        taken.1 += memory_mib;
        Ok(Taken(vcpus, memory_mib))
    }
}

impl Drop for Taken {
    fn drop(&mut self) {
        let mut taken = TAKEN.lock().unwrap();
        taken.0 -= self.0;
        taken.1 -= self.1;
    }
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
    /// The files of the volumes, over WebDAV, and of the sandboxes, through their machines.
    files: Files,
    /// What a files request is writing now, and how many write each: neither let go nor kept
    /// meanwhile ([`Home::serve_files`]).
    writing: Arc<Mutex<HashMap<(Kind, String), usize>>>,
    /// The lazy snapshots being filled ahead of their machine ([`Home::fill_ahead`]), each
    /// with what stops it.
    filling: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>>,
    /// This binary, as each machine is spawned.
    vm: Vec<OsString>,
    /// Every sandbox, running or stopped, until it is deleted.
    sandboxes: Sandboxes,
    /// The image jobs, by id, and what cancels each, until each is removed or a while after
    /// it ends.
    image_jobs: Mutex<HashMap<String, (Arc<ImageJobOf>, AbortHandle)>>,
    /// Where its snapshots, volumes and images are kept past this server, if anywhere.
    keeping: Option<Arc<dyn Keeping>>,
    /// Its sandboxes' terminals, kept across connections.
    pub terminals: Arc<crate::terminals::Sessions>,
}

impl Home {
    /// The images in `images` -- its own, or shared with other projects -- and the snapshots,
    /// volumes and sandboxes under `root`, kept past this server by what `keeping` makes, given
    /// one.
    pub fn new(
        images: ImageStore,
        root: &Path,
        keeping: Option<MakeKeeping>,
    ) -> anyhow::Result<Home> {
        let snapshots = SnapshotStore::new(root)?;
        let volumes = VolumeStore::new(root)?;
        let keeping = keeping.map(|make| make(&snapshots, &volumes)).transpose()?;
        let mut registry = RegistryServer::new(images.clone(), "/images/oci")?;
        if let Some(keeping) = &keeping {
            registry = registry.listing_too(keeping.listed_too());
        }
        let vm: Vec<OsString> = vec![
            std::env::current_exe()?.into(),
            crate::serve::MACHINE_SUBCOMMAND.into(),
        ];
        let hooks = Arc::new(HomeHooks {
            keeping: keeping.clone(),
            home: OnceLock::new(),
        });
        let sandboxes = Sandboxes::new(
            images.clone(),
            snapshots.clone(),
            volumes.clone(),
            root,
            vm.clone(),
            Arc::clone(&hooks) as Arc<dyn Hooks>,
        )?;
        let home = Home(Arc::new(HomeInner {
            registry: Arc::new(registry),
            images,
            files: Files::new(snapshots.clone(), volumes.clone()),
            writing: Arc::default(),
            filling: Arc::default(),
            snapshots,
            volumes,
            vm,
            sandboxes,
            image_jobs: Mutex::default(),
            keeping,
            terminals: Arc::default(),
        }));
        let _ = hooks.home.set(Arc::downgrade(&home.0));
        Ok(home)
    }

    /// Stop every sandbox, its machine with it, in a thread of its own: what they wrote is on
    /// their disks once each has ended, and each is kept, stopped, for a server started again --
    /// but an ephemeral one, deleted.
    pub fn close_all(&self) {
        self.sandboxes.close_all();
    }

    /// As [`close_all`](Self::close_all), blocking until each has stopped: not on an async
    /// runtime's thread.
    pub fn close_all_blocking(&self) {
        self.sandboxes.close_all_blocking();
    }

    /// A request for the project, carrying `token`: what its keeping needs of it, if it has
    /// one ([`Keeping::requested`]).
    pub async fn requested(self: &Arc<Self>, token: &str) {
        if let Some(keeping) = &self.keeping {
            keeping.requested(self, token).await;
        }
    }

    /// Tag `reference` brought up to its storage home's index before it is used, if the image
    /// it names there is not here yet.
    pub async fn want(&self, reference: &str) -> anyhow::Result<()> {
        match &self.keeping {
            Some(keeping) => keeping.want(self, reference).await,
            None => Ok(()),
        }
    }

    /// Image `digest` fetched from its storage home before it is used, if it is kept there and
    /// not here yet.
    pub async fn want_image(&self, digest: &Digest) -> anyhow::Result<()> {
        match &self.keeping {
            Some(keeping) => keeping.want_image(self, digest).await,
            None => Ok(()),
        }
    }

    /// The digest and manifest of the image tag `reference` names in its storage home, if it is
    /// not here yet: what a `HEAD` of the tag answers without fetching the image.
    pub async fn wanted_manifest(
        &self,
        reference: &str,
    ) -> anyhow::Result<Option<(Digest, String)>> {
        match &self.keeping {
            Some(keeping) => keeping.wanted_manifest(reference).await,
            None => Ok(None),
        }
    }

    /// The manifest of image `digest` where it is kept, if it is not here yet.
    pub async fn kept_manifest(&self, digest: &Digest) -> anyhow::Result<Option<String>> {
        match &self.keeping {
            Some(keeping) => keeping.kept_manifest(digest).await,
            None => Ok(None),
        }
    }

    /// Blob `digest` where it is kept, if it is small and not here yet.
    pub async fn kept_blob(&self, digest: &Digest) -> anyhow::Result<Option<Vec<u8>>> {
        match &self.keeping {
            Some(keeping) => keeping.kept_blob(digest).await,
            None => Ok(None),
        }
    }

    /// Its images, as an OCI registry.
    pub fn registry(&self) -> Arc<RegistryServer> {
        Arc::clone(&self.registry)
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

    /// What its sandboxes write, each one's snapshot and the volumes it may write, and what a
    /// files request is writing: neither let go nor kept while they do.
    pub fn used(&self) -> HashSet<(Kind, String)> {
        let mut used: HashSet<_> = self.writing.lock().unwrap().keys().cloned().collect();
        used.extend(self.sandboxes.writes().into_iter().map(kind_of));
        used
    }

    /// Whether a files request is writing `kind` `name` now.
    pub fn is_written(&self, kind: Kind, name: &str) -> bool {
        self.writing
            .lock()
            .unwrap()
            .contains_key(&(kind, name.to_owned()))
    }

    /// Whether this home's snapshots and volumes are kept past this server.
    pub fn is_kept(&self) -> bool {
        self.keeping.is_some()
    }

    /// How many sandboxes run in this home, and how many jobs run in them now.
    pub fn counts(&self) -> (usize, usize) {
        self.sandboxes.counts()
    }

    /// A sandbox made of `args` under the id `id`, its ports behind `capabilities` (port by
    /// capability), as one made elsewhere was -- what its client holds, its id and its port
    /// URLs, naming it here too -- and started, over its disk as it is kept, unless not
    /// `booted`: then made, and its disk brought here, for it to start at once when it is next
    /// asked to. One here under that id already is started, if it is stopped and `booted`, and
    /// answered.
    pub async fn open_as(
        &self,
        id: String,
        args: SandboxCreateArgs,
        capabilities: HashMap<String, u16>,
        booted: bool,
    ) -> Result<virtx_core::Sandbox, Refused> {
        if !id_like(&id) {
            return Err(invalid("id: not a sandbox's id"));
        }
        if let Some(capability) = capabilities.keys().find(|c| !word(c, 128)) {
            return Err(invalid(format!("capabilities: {capability} is not one")));
        }
        if let Ok(here) = self.sandboxes.get(&id) {
            return match booted {
                false => Ok(here.answer(true)),
                true => Ok(here.start().await?),
            };
        }
        let given = Given {
            id,
            keys: capabilities,
        };
        Ok(self.sandboxes.create(args, Some(given), booted).await?)
    }

    /// Sandbox `id`, stopped, let go of here, its disk left as it is kept: its record and its
    /// jobs gone, as a delete does, but nothing removed from the storage home -- what a server
    /// does once the sandbox is to run elsewhere, which takes its disk from there. Refused
    /// `409`, `running`, while it runs; `404` for one this server does not have.
    pub async fn let_go(&self, id: &str) -> Result<(), Refused> {
        Ok(self.sandboxes.forget(id).await?)
    }

    /// `cmd` on a terminal of its own, `cols` by `rows`, in sandbox `id`: a job of it, as an
    /// exec is, which keeps it in use while it runs, and ends with the [`Terminal`].
    pub async fn terminal(
        &self,
        id: &str,
        cmd: Vec<String>,
        cols: u16,
        rows: u16,
    ) -> Result<Terminal, Refused> {
        let sandbox = self.sandboxes.get(id)?;
        Ok(sandbox.terminal(cmd, cols, rows).await?)
    }

    /// `kind` `name` may be changed now: kept again once nothing has it.
    fn changed(&self, kind: Kind, name: &str) -> Result<(), Refused> {
        match &self.keeping {
            Some(keeping) => keeping
                .changed(kind, name)
                .map_err(|e| internal(format!("{e:#}"))),
            None => Ok(()),
        }
    }

    /// `kind` `name` was just made here, `disk` bytes: in the storage home too, if there is
    /// one -- refused, and removed here, if another node's of that name is there.
    async fn made(&self, kind: Kind, name: &str, disk: u64) -> Result<(), Refused> {
        let Some(keeping) = &self.keeping else {
            return Ok(());
        };
        match keeping.made(kind, name, disk).await {
            Ok(()) => Ok(()),
            Err(e) => {
                let _ = match kind {
                    Kind::Snapshot => self.snapshots.remove(name).map_err(|_| ()),
                    Kind::Volume => self.volumes.remove(name).map_err(|_| ()),
                };
                Err(e.into())
            }
        }
    }

    /// Hold `kind` `name` here to write it, brought up to what is kept: refused while another
    /// node holds it. Nothing to do without a storage home.
    async fn take(&self, kind: Kind, name: &str) -> Result<(), Refused> {
        match &self.keeping {
            Some(keeping) => Ok(keeping.take(self, kind, name).await?),
            None => Ok(()),
        }
    }

    /// As [`take`](Self::take), to boot a machine on: lazily, if the keeping can.
    async fn take_to_boot(&self, kind: Kind, name: &str) -> Result<(), Refused> {
        match &self.keeping {
            Some(keeping) => Ok(keeping.take_to_boot(self, kind, name).await?),
            None => Ok(()),
        }
    }

    /// Snapshot `name`, lazy, fetched whole in the background, held meanwhile: as a disk made
    /// ready ahead of its start is. A start, a removal, or a reader that wants it whole stops
    /// or waits on it ([`settle`](Self::settle)); a machine fetches what is left as it reads.
    pub fn fill_ahead(&self, name: &str) {
        #[cfg(unix)]
        {
            let Some(keeping) = self.keeping.clone() else {
                return;
            };
            let Ok(snapshot) = self.snapshots.open(name) else {
                return;
            };
            if !virtx_uvm::lazy::is_lazy(&snapshot.path) {
                return;
            }
            let stop = Arc::new(AtomicBool::new(false));
            {
                let mut filling = self.filling.lock().unwrap();
                if filling.contains_key(name) {
                    return;
                }
                filling.insert(name.to_owned(), Arc::clone(&stop));
            }
            let source = Arc::new(KeptChunks {
                keeping,
                kind: Kind::Snapshot,
                name: name.to_owned(),
                runtime: tokio::runtime::Handle::current(),
            });
            let (filling, name) = (Arc::clone(&self.filling), name.to_owned());
            tokio::task::spawn_blocking(move || {
                let filled = match snapshot.lock() {
                    Ok(held) => {
                        let filled =
                            virtx_uvm::lazy::materialize_until(&snapshot.path, source, &stop);
                        drop(held);
                        filled.map(|_| ())
                    }
                    // A machine on it already, which fetches it as it reads.
                    Err(_) => Ok(()),
                };
                filling.lock().unwrap().remove(&name);
                if let Err(e) = filled {
                    eprintln!("snapshot {name} not filled ahead: {e}");
                }
            });
        }
        #[cfg(not(unix))]
        let _ = name;
    }

    /// Snapshot `name`'s fill ahead, if one is going, stopped -- what it fetched kept -- and
    /// waited for until it has let go of the disk.
    pub async fn settle(&self, name: &str) {
        let stop = self.filling.lock().unwrap().get(name).cloned();
        let Some(stop) = stop else {
            return;
        };
        stop.store(true, Ordering::Release);
        while self.filling.lock().unwrap().contains_key(name) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// `kind` `name`'s disk at `path` -- its own, or a copy of it -- made whole if it is lazy:
    /// every chunk not here fetched from where it is kept. For a reader of the disk other than
    /// its machine. Its own, held meanwhile; one a machine runs on is left to the machine,
    /// which fills it.
    pub async fn materialize(&self, kind: Kind, name: &str, path: &Path) -> Result<(), Refused> {
        #[cfg(unix)]
        if let Some(keeping) = &self.keeping
            && virtx_uvm::lazy::is_lazy(path)
        {
            // Filled ahead now: let that make it whole, rather than find it held.
            while self.filling.lock().unwrap().contains_key(name) {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            let own = self
                .snapshots
                .open(name)
                .ok()
                .filter(|own| own.path == path);
            let held = match own.map(|own| own.lock()).transpose() {
                Ok(held) => held,
                Err(SnapshotError::InUse(_)) => return Ok(()),
                Err(e) => return Err(e.into()),
            };
            let source = Arc::new(KeptChunks {
                keeping: Arc::clone(keeping),
                kind,
                name: name.to_owned(),
                runtime: tokio::runtime::Handle::current(),
            });
            let path = path.to_owned();
            tokio::task::spawn_blocking(move || {
                let _held = held;
                virtx_uvm::lazy::materialize(&path, source)
            })
            .await?
            .map_err(|e| {
                Refusal::Failed(anyhow::anyhow!(
                    "fetching {} {name} whole: {e}",
                    kind.as_str()
                ))
            })?;
        }
        #[cfg(not(unix))]
        let _ = (kind, name, path);
        Ok(())
    }

    /// `kind` `name`'s chunks answered on a socket of its own, its disk at `path` being lazy:
    /// until what this answers drops. `None` for a disk that is not.
    fn serve_chunks(
        &self,
        kind: Kind,
        name: &str,
        path: &Path,
    ) -> Result<Option<ChunksServed>, Refused> {
        #[cfg(unix)]
        if let Some(keeping) = &self.keeping
            && virtx_uvm::lazy::is_lazy(path)
        {
            let socket = virtx_uvm::lazy::chunks_socket(name);
            let _ = std::fs::remove_file(&socket);
            let listener = tokio::net::UnixListener::bind(&socket).map_err(|e| {
                Refusal::Failed(anyhow::anyhow!("listening at {}: {e}", socket.display()))
            })?;
            let (keeping, name) = (Arc::clone(keeping), name.to_owned());
            let task = tokio::spawn(async move {
                while let Ok((stream, _)) = listener.accept().await {
                    let (keeping, name) = (Arc::clone(&keeping), name.clone());
                    tokio::spawn(answer_chunk(stream, keeping, kind, name));
                }
            });
            return Ok(Some(ChunksServed { socket, task }));
        }
        #[cfg(not(unix))]
        let _ = (kind, name, path);
        Ok(None)
    }

    /// `kind` `name` brought up to what is kept, only to be read.
    async fn bring_up(&self, kind: Kind, name: &str) -> Result<(), Refused> {
        match &self.keeping {
            Some(keeping) => Ok(keeping.bring_up(self, kind, name).await?),
            None => Ok(()),
        }
    }

    /// `kind` `name`, to be removed: from the storage home first, which refuses while another
    /// node holds it.
    async fn remove_kept(&self, kind: Kind, name: &str) -> Result<(), Refused> {
        match &self.keeping {
            Some(keeping) => Ok(keeping.remove(self, kind, name).await?),
            None => Ok(()),
        }
    }

    /// Answer `request`, to the files of a volume.
    ///
    /// With a storage home, the copy here is what is kept there, as for a sandbox: brought up
    /// to what is kept for a request that reads, and held for one that writes -- refused,
    /// `423`, while another node holds it. One written is kept again once the write is done.
    pub async fn serve_files(&self, request: axum::extract::Request) -> Response {
        let held = |name: &str| self.holds(name);
        let (Some(keeping), Some((kind, name))) = (&self.keeping, files_root(request.uri().path()))
        else {
            return self.files.handle(request, &held).await;
        };
        let writes = !matches!(
            request.method().as_str(),
            "GET" | "HEAD" | "OPTIONS" | "PROPFIND"
        );
        let ready = match writes {
            true => keeping.take(self, kind, &name).await,
            false => keeping.bring_up(self, kind, &name).await,
        };
        if let Err(e) = ready {
            let status = match &e {
                Refusal::Held { .. } => StatusCode::LOCKED,
                Refusal::Unknown { .. } => StatusCode::NOT_FOUND,
                Refusal::Exists { .. } | Refusal::Failed(_) => StatusCode::INTERNAL_SERVER_ERROR,
            };
            return (status, format!("{e:#}")).into_response();
        }
        if !writes {
            return self.files.handle(request, &held).await;
        }
        let _writing = Writing::start(&self.writing, kind, &name);
        let response = self.files.handle(request, &held).await;
        if response.status().is_success()
            && let Err(e) = keeping.changed(kind, &name)
        {
            eprintln!(
                "{} {name} written over WebDAV, not to be kept: {e:#}",
                kind.as_str()
            );
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
        response
    }

    /// Stop each sandbox that has run nothing, and been reached through nothing, for its
    /// `auto_stop_minutes`; and delete each kept stopped and unused for its `auto_delete_days`
    /// ([`Sandboxes::tend`]).
    pub fn tend(&self) {
        self.sandboxes.tend();
    }

    /// Stop every sandbox here, in use or not: what runs in each ends, and its disk is kept
    /// as it stopped -- a draining server told not to wait for what runs.
    pub async fn stop_all(&self) {
        self.sandboxes.stop_all().await;
    }

    /// Stop every sandbox that runs nothing and is reached at none of its ports now, whatever
    /// its `auto_stop_minutes`: what a node draining lets go of before it goes. A terminal open
    /// in one does not keep it.
    pub async fn stop_unused(&self) {
        self.sandboxes.stop_unused().await;
    }

    /// A copy of the disk of snapshot `name`, which a sandbox here stands on, at `to`, as its
    /// machine has it: what the guest has written put on it first, and the machine paused while
    /// it is copied -- for as long as that takes -- so that the copy is whole. `false`, and no
    /// copy, where no sandbox here stands on it with its machine up.
    #[cfg(unix)]
    pub async fn copy_snapshot(&self, name: &str, to: &Path) -> Result<bool, Refused> {
        Ok(self.copy_snapshot_marked(name, to).await?.is_some())
    }

    /// As [`copy_snapshot`](Self::copy_snapshot), the disk marked as it is copied: once the
    /// copy is kept, what it was kept as can be made the disk's base
    /// ([`rebase`](Self::rebase)) -- the mark's, if the disk could be marked. `None`, and no
    /// copy, where no sandbox here stands on it with its machine up.
    #[cfg(unix)]
    pub async fn copy_snapshot_marked(
        &self,
        name: &str,
        to: &Path,
    ) -> Result<Option<Option<u64>>, Refused> {
        match self.sandboxes.running_on(name) {
            Some(sandbox) => Ok(sandbox.copy_disk(to).await?),
            None => Ok(None),
        }
    }

    /// What the copy of snapshot `name` made at `mark` ([`copy_snapshot_marked`]) was kept as,
    /// `kept`, made its disk's base while its machine runs: what its next save reads is only
    /// what was written since the mark. Whether it was: not if its machine is down now -- its
    /// next save reads what it would have -- or another mark came since.
    ///
    /// [`copy_snapshot_marked`]: Self::copy_snapshot_marked
    #[cfg(unix)]
    pub async fn rebase(
        &self,
        name: &str,
        mark: u64,
        kept: &virtx_uvm::lazy::Sidecar,
    ) -> Result<bool, Refused> {
        match self.sandboxes.running_on(name) {
            Some(sandbox) => Ok(sandbox.rebase(mark, kept).await?),
            None => Ok(false),
        }
    }

    /// A tunnel to the port `capability` opens, in whichever of this home's sandboxes it is
    /// one of. None while it is stopped: a connection to a stopped sandbox's port does not
    /// start it.
    pub fn tunnel(&self, capability: &str) -> Option<Tunnel> {
        let (sandbox, port) = self.sandboxes.find_port(capability)?;
        Some(Tunnel {
            activity: sandbox.activity(),
            port,
        })
    }

    /// Whether a sandbox running here has the disk `name`, and so holds it.
    pub fn holds(&self, name: &str) -> bool {
        self.sandboxes.running_on(name).is_some()
    }

    /// The sandbox `id`, running or stopped.
    fn sandbox(&self, id: &str) -> Result<Arc<Sandbox>, Refused> {
        Ok(self.sandboxes.get(id)?)
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

/// What a home plugs into its sandboxes: their disks and volumes kept past this server by its
/// keeping, their machines held to the server's [`Capacity`], and their ports named by
/// capabilities.
struct HomeHooks {
    keeping: Option<Arc<dyn Keeping>>,
    /// The home, once it is made.
    home: OnceLock<Weak<HomeInner>>,
}

impl HomeHooks {
    fn home(&self) -> Result<Home, SandboxError> {
        self.home
            .get()
            .and_then(Weak::upgrade)
            .map(Home)
            .ok_or_else(|| SandboxError::Failed("the home is gone".into()))
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

/// `written`, as the keeping names it.
fn kind_of(written: Written) -> (Kind, String) {
    match written {
        Written::Disk(name) => (Kind::Snapshot, name),
        Written::Volume(name) => (Kind::Volume, name),
    }
}

impl Hooks for HomeHooks {
    fn want<'a>(&'a self, wanted: Wanted<'a>) -> BoxFuture<'a, Result<(), SandboxError>> {
        Box::pin(async move {
            let home = self.home()?;
            let fetched = match wanted {
                Wanted::Tag(tag) => home.want(tag).await,
                Wanted::Image(digest) => home.want_image(digest).await,
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

    fn made<'a>(&'a self, id: &'a str, size: u64) -> BoxFuture<'a, Result<(), SandboxError>> {
        Box::pin(async move {
            self.home()?
                .made(Kind::Snapshot, id, size)
                .await
                .map_err(hooked)
        })
    }

    fn adopted<'a>(
        &'a self,
        id: &'a str,
        booted: bool,
    ) -> BoxFuture<'a, Result<Option<bool>, SandboxError>> {
        Box::pin(async move {
            let Some(keeping) = &self.keeping else {
                return Ok(None);
            };
            let home = self.home()?;
            let taking = std::time::Instant::now();
            let taken = keeping.take_to_boot(&home, Kind::Snapshot, id).await;
            eprintln!(
                "reopened {id} ({}): its disk taken in {} ms",
                if booted { "to boot" } else { "ahead" },
                taking.elapsed().as_millis()
            );
            match taken {
                Ok(()) => Ok(Some(true)),
                Err(Refusal::Unknown { .. }) => Ok(Some(false)),
                Err(e) => Err(hooked(e)),
            }
        })
    }

    fn readied(&self, id: &str) {
        if let Ok(home) = self.home() {
            home.fill_ahead(id);
        }
    }

    /// Room for its machine, held until it stops; its disk held here and brought up to what is
    /// kept -- lazily, if the keeping can -- and each volume it writes held, each it only reads
    /// brought up: refused while another node holds either. A fill ahead of its start, stopped.
    fn booting<'a>(
        &'a self,
        sandbox: &'a Sandbox,
        spec: &'a MachineSpec,
    ) -> BoxFuture<'a, Result<Vec<Held>, SandboxError>> {
        Box::pin(async move {
            let taken = Taken::of(spec).map_err(hooked)?;
            let home = self.home()?;
            let id = sandbox.id();
            home.take_to_boot(Kind::Snapshot, id)
                .await
                .map_err(hooked)?;
            for asked in &sandbox.args().volumes {
                match asked.readonly {
                    true => home.bring_up(Kind::Volume, &asked.volume).await,
                    false => home.take(Kind::Volume, &asked.volume).await,
                }
                .map_err(hooked)?;
            }
            home.settle(id).await;
            Ok(vec![Box::new(taken) as Held])
        })
    }

    fn changed(&self, writes: &[Written]) -> Result<(), SandboxError> {
        let home = self.home()?;
        for (kind, name) in writes.iter().cloned().map(kind_of) {
            home.changed(kind, &name).map_err(hooked)?;
        }
        Ok(())
    }

    fn chunks(&self, id: &str, disk: &Path) -> Result<Option<(PathBuf, Held)>, SandboxError> {
        let served = self
            .home()?
            .serve_chunks(Kind::Snapshot, id, disk)
            .map_err(hooked)?;
        Ok(served.map(|served| (served.socket.clone(), Box::new(served) as Held)))
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
            self.home()?
                .materialize(Kind::Snapshot, id, disk)
                .await
                .map_err(hooked)
        })
    }

    fn reading<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<(), SandboxError>> {
        Box::pin(async move {
            self.home()?
                .bring_up(Kind::Snapshot, id)
                .await
                .map_err(hooked)
        })
    }

    /// Its disk removed from the storage home first, if there is one -- one never kept there,
    /// made and deleted before it was, is nothing to remove -- and a fill ahead of it stopped.
    fn deleting<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<(), SandboxError>> {
        Box::pin(async move {
            let home = self.home()?;
            if let Some(keeping) = &self.keeping {
                match keeping.remove(&home, Kind::Snapshot, id).await {
                    Ok(()) | Err(Refusal::Unknown { .. }) => {}
                    Err(e) => return Err(hooked(e)),
                }
            }
            home.settle(id).await;
            Ok(())
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

/// Whether `s` is letters and digits alone, one to `most` of them.
fn word(s: &str, most: usize) -> bool {
    !s.is_empty() && s.len() <= most && s.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// Whether `s` can be a sandbox's id: a name, never a path -- letters and digits, and `_` or
/// `-` after the first, 64 at most.
fn id_like(s: &str) -> bool {
    s.len() <= 64
        && s.bytes().next().is_some_and(|b| b.is_ascii_alphanumeric())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
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
        .route("/sandboxes/", get(sandbox_list).post(sandbox_create))
        .route("/sandboxes/{id}", get(sandbox_get).delete(sandbox_remove))
        .route("/sandboxes/{id}/start", post(sandbox_start))
        .route("/sandboxes/{id}/stop", post(sandbox_stop))
        .route(
            "/sandboxes/{id}/ports/{port}",
            put(sandbox_port_open).delete(sandbox_port_close),
        )
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
/// ends or is cancelled, and kept [`IMAGE_JOB_KEPT`] after.
fn image_job<Fut>(
    home: &Arc<Home>,
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
            let registry = registry(images.clone(), args.auth);
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
            registry(images.clone(), None)
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
    let sandbox = home.sandbox(&args.sandbox)?;
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

async fn volume_list(Extension(home): Extension<Arc<Home>>, page: Query<Page>) -> Answer {
    if let Some(keeping) = &home.keeping {
        let kept = keeping
            .list(Kind::Volume, page.after.as_deref(), page.limit)
            .await
            .map_err(|e| internal(format!("{e:#}")))?;
        let listed: Vec<_> = kept
            .into_iter()
            .map(|v| VolumeListOutputItem {
                name: v.name,
                size: v.size,
            })
            .collect();
        return Ok(json(&listed));
    }
    let mut listed = Vec::new();
    for name in page.of(home.volumes.list()?, |n| n) {
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
    home.made(Kind::Volume, &name, 0).await?;
    Ok(created(format!("/volumes/{name}"), &volume(&home, name)?))
}

async fn volume_get(
    Extension(home): Extension<Arc<Home>>,
    Segments(name): Segments<String>,
) -> Answer {
    if let Some(keeping) = &home.keeping {
        let kept = keeping
            .get(Kind::Volume, &name)
            .await
            .map_err(|e| internal(format!("{e:#}")))?
            .ok_or(VolumeError::Unknown(name))?;
        return Ok(json(&VolumeListOutputItem {
            name: kept.name,
            size: kept.size,
        }));
    }
    Ok(json(&volume(&home, name)?))
}

async fn volume_remove(
    Extension(home): Extension<Arc<Home>>,
    Segments(name): Segments<String>,
) -> Answer {
    if let Ok(volume) = home.volumes.open(&name) {
        // Mounted here: refused before anything is removed.
        drop(volume.hold_alone()?);
    }
    home.remove_kept(Kind::Volume, &name).await?;
    match home.volumes.remove(&name) {
        Ok(()) => {}
        Err(VolumeError::Unknown(_)) if home.keeping.is_some() => {}
        Err(e) => return Err(e.into()),
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn volume_mount_list(
    Extension(home): Extension<Arc<Home>>,
    Segments(name): Segments<String>,
) -> Answer {
    home.bring_up(Kind::Volume, &name).await?;
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
    // Held, and kept again after: its mounts go with it to the next node that takes it.
    home.take(Kind::Volume, &name).await?;
    home.volumes.open(&name)?.mount(path.clone(), args.source)?;
    home.changed(Kind::Volume, &name)?;
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
    home.take(Kind::Volume, &name).await?;
    home.volumes.open(&name)?.unmount(&path)?;
    home.changed(Kind::Volume, &name)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// A request to the files of a volume, over WebDAV.
async fn files(Extension(home): Extension<Arc<Home>>, request: axum::extract::Request) -> Response {
    home.serve_files(request).await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DiskArgs {
    disk_gib: u32,
}

/// `PUT /sandboxes/{id}/disk`, `{"disk_gib": n}`: its disk grown to `n` GiB -- never shrunk:
/// smaller than it is, refused `400`. Grown as it next starts, what is on it kept
/// ([`virtx_uvm::Snapshot::grow`]). Answered `{disk_gib, running}`: `running`, it is grown once
/// it starts again.
async fn sandbox_disk(
    Extension(home): Extension<Arc<Home>>,
    Segments(id): Segments<String>,
    request: Bytes,
) -> Answer {
    let args: DiskArgs = body(&request)?;
    let running = home.sandbox(&id)?.grow_disk(args.disk_gib)?;
    Ok(json(&serde_json::json!({
        "disk_gib": args.disk_gib,
        "running": running,
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
/// machine resized, and its disk grown -- as it next starts. Its vCPUs and memory only while it
/// is stopped, refused `409`, `running`, else: a machine is the size it booted as. Its disk
/// never shrunk, refused `400`. Answered `{vcpus, memory_mib, disk_gib, running}`, as it is to
/// be.
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

/// A request to the files of a sandbox, over WebDAV, asked of its machine while it runs: one
/// that is stopped answers `409`, `stopped`, and is not started for it.
async fn sandbox_files(
    Extension(home): Extension<Arc<Home>>,
    Segments(segments): Segments<HashMap<String, String>>,
    request: axum::extract::Request,
) -> Answer {
    let id = &segments["id"];
    let sandbox = home.sandbox(id)?;
    // In use from here until the answer has been sent.
    let activity = sandbox.activity();
    let machine = sandbox.running_machine()?;
    Ok(home.files.sandbox(id, machine, activity, request).await)
}

#[derive(Deserialize)]
struct Named {
    name: Option<String>,
}

/// Every sandbox, running or stopped; `?name=` those named so, one at most.
async fn sandbox_list(Extension(home): Extension<Arc<Home>>, named: Query<Named>) -> Answer {
    Ok(json(&home.sandboxes.list(named.name.as_deref())))
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
    Ok(json(&home.sandbox(&id)?.answer(false)))
}

/// Started if it is stopped; answered with where its ports are.
async fn sandbox_start(
    Extension(home): Extension<Arc<Home>>,
    Segments(id): Segments<String>,
) -> Answer {
    Ok(json(&home.sandbox(&id)?.start().await?))
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

/// Port `port` of sandbox `id` opened, as those it was made with are: answered as each of
/// those was, `201` and where it is reached -- or `200` and where, for one open already. A
/// program in it that listens on the port already is reached once it listens again.
async fn sandbox_port_open(
    Extension(home): Extension<Arc<Home>>,
    Segments((id, port)): Segments<(String, u16)>,
    request: Bytes,
) -> Answer {
    // None, `null` or `{}`: TCP, named nothing.
    let asked: SandboxPortOpenArgs = match request.iter().all(u8::is_ascii_whitespace) {
        true => SandboxPortOpenArgs::default(),
        false => serde_json::from_slice::<Option<SandboxPortOpenArgs>>(&request)
            .map_err(invalid)?
            .unwrap_or_default(),
    };
    let sandbox = home.sandbox(&id)?;
    match sandbox.open_port(port, asked.label, asked.protocol).await? {
        (open, true) => Ok(created(format!("/sandboxes/{id}/ports/{port}"), &open)),
        (open, false) => Ok(json(&open)),
    }
}

/// Port `port` of sandbox `id` closed: its capability reaches it no more from now on, and it is
/// not opened again as the sandbox starts. `404` for one not open. A connection through it open
/// now runs on until it ends.
async fn sandbox_port_close(
    Extension(home): Extension<Arc<Home>>,
    Segments((id, port)): Segments<(String, u16)>,
) -> Answer {
    home.sandbox(&id)?.close_port(port)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn sandbox_job_list(
    Extension(home): Extension<Arc<Home>>,
    Segments(id): Segments<String>,
) -> Answer {
    Ok(json(&home.sandbox(&id)?.jobs()))
}

/// The header an exec answered with its log carries its job in, as JSON.
pub const JOB_HEADER: &str = "virtx-job";

/// `POST /sandboxes/{id}/jobs/exec/`. One that accepts `text/event-stream` is answered `201`
/// with the job's log as the body, the job in [`JOB_HEADER`] and its path in `Location`: its
/// output and end with no second request. Any other is answered with the job.
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
    let sandbox = home.sandbox(&id)?;
    let timeout = args.timeout_ms.map(Duration::from_millis);
    // A job started in a stopped sandbox starts it first.
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
    request: Bytes,
) -> Answer {
    let args: SandboxSignalArgs = body(&request)?;
    let signal = signal_number(&args.signal)
        .ok_or_else(|| invalid(format!("signal: no signal is named {}", args.signal)))?;
    home.sandbox(&id)?.signal(&job, signal)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// Write to the stdin of a job started with one, and close it if `eof`. Refused `409`, `ended`,
/// for one started without, one whose stdin is closed, and one that has ended.
async fn sandbox_job_stdin(
    Extension(home): Extension<Arc<Home>>,
    Segments((id, job)): Segments<(String, String)>,
    request: Bytes,
) -> Answer {
    let args: SandboxStdinArgs = body(&request)?;
    let sandbox = home.sandbox(&id)?;
    sandbox.write_stdin(&job, args.data.unwrap_or_default(), args.eof)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn sandbox_job_events(
    Extension(home): Extension<Arc<Home>>,
    Segments((id, job)): Segments<(String, String)>,
    headers: HeaderMap,
) -> Answer {
    Ok(events(home.sandbox(&id)?.job(&job)?.log(), &headers))
}

/// A lazy disk's chunks answered to its machine ([`Home::serve_chunks`]): the socket, and what
/// answers on it, stopped and gone once this drops.
struct ChunksServed {
    socket: PathBuf,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for ChunksServed {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_file(&self.socket);
    }
}

/// The chunk asked for on `stream` answered, from where `kind` `name` is kept.
#[cfg(unix)]
async fn answer_chunk(
    mut stream: tokio::net::UnixStream,
    keeping: Arc<dyn Keeping>,
    kind: Kind,
    name: String,
) {
    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};
    let mut asked = String::new();
    let mut reading = tokio::io::BufReader::new(&mut stream);
    if reading.read_line(&mut asked).await.is_err() {
        return;
    }
    let chunk = asked.trim_end();
    let answer = match keeping.chunk(kind, &name, chunk).await {
        Ok(bytes) => virtx_uvm::lazy::answer(Ok(&bytes)),
        Err(e) => {
            eprintln!("chunk {chunk} of {} {name}: {e:#}", kind.as_str());
            virtx_uvm::lazy::answer(Err(&format!("{e:#}")))
        }
    };
    let _ = stream.write_all(&answer).await;
}

/// Chunks of `kind` `name` fetched from where they are kept, from a thread of its own: what
/// [`Home::materialize`] fetches with.
#[cfg(unix)]
struct KeptChunks {
    keeping: Arc<dyn Keeping>,
    kind: Kind,
    name: String,
    runtime: tokio::runtime::Handle,
}

#[cfg(unix)]
impl virtx_uvm::lazy::ChunkSource for KeptChunks {
    fn fetch(&self, chunk: &str) -> io::Result<Vec<u8>> {
        self.runtime
            .block_on(self.keeping.chunk(self.kind, &self.name, chunk))
            .map_err(|e| io::Error::other(format!("{e:#}")))
    }
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

/// Whose files a path under the files endpoints names, and its name.
fn files_root(path: &str) -> Option<(Kind, String)> {
    let (kind, rest) = match (
        path.strip_prefix("/volumes/"),
        path.strip_prefix("/snapshots/"),
    ) {
        (Some(rest), _) => (Kind::Volume, rest),
        (_, Some(rest)) => (Kind::Snapshot, rest),
        _ => return None,
    };
    let name = rest.split('/').next().unwrap_or("");
    (!name.is_empty()).then(|| (kind, name.to_owned()))
}

/// A files request writing, counted in [`Home::writing`] until it is dropped: when it ends,
/// or when it is cancelled.
struct Writing {
    writing: Arc<Mutex<HashMap<(Kind, String), usize>>>,
    key: (Kind, String),
}

impl Writing {
    fn start(
        writing: &Arc<Mutex<HashMap<(Kind, String), usize>>>,
        kind: Kind,
        name: &str,
    ) -> Writing {
        let key = (kind, name.to_owned());
        *writing.lock().unwrap().entry(key.clone()).or_default() += 1;
        Writing {
            writing: Arc::clone(writing),
            key,
        }
    }
}

impl Drop for Writing {
    fn drop(&mut self) {
        let mut writing = self.writing.lock().unwrap();
        if let Some(count) = writing.get_mut(&self.key) {
            *count -= 1;
            if *count == 0 {
                writing.remove(&self.key);
            }
        }
    }
}

/// A page of a listing, the cloud's extension: those after `after` by name, `limit` at most.
/// Neither, and the whole listing.
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Page {
    after: Option<String>,
    limit: Option<u32>,
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

impl Refused {
    /// What it is answered with.
    pub fn status(&self) -> StatusCode {
        self.0
    }

    /// Its `code` and `message`, as its body's `error` says them.
    pub fn said(&self) -> serde_json::Value {
        serde_json::json!({"code": self.1, "message": self.2})
    }
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

impl From<Refusal> for Refused {
    fn from(e: Refusal) -> Refused {
        match &e {
            Refusal::Held { .. } => refused(StatusCode::CONFLICT, "in_use", format!("{e:#}")),
            Refusal::Unknown { .. } => not_found(format!("{e:#}")),
            Refusal::Exists { .. } => refused(StatusCode::CONFLICT, "exists", format!("{e:#}")),
            Refusal::Failed(_) => internal(format!("{e:#}")),
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn machines_past_the_capacity_are_refused_until_one_ends() {
        set_capacity(Capacity {
            vcpus: 4,
            memory_mib: 8192,
        });
        let spec = MachineSpec {
            vcpus: 2,
            ram_mib: 4096,
            gpu: false,
            gpu_memory_mib: None,
            network: Network::None,
        };
        let one = Taken::of(&spec).ok().expect("room for one");
        let _two = Taken::of(&spec).ok().expect("room for two");
        let refused = Taken::of(&spec).err().expect("no room for a third");
        assert_eq!(
            (refused.0, refused.1),
            (StatusCode::SERVICE_UNAVAILABLE, "capacity")
        );
        drop(one);
        assert!(Taken::of(&spec).is_ok());
    }

    #[test]
    fn a_volume_written_is_neither_let_go_nor_kept_until_the_write_ends() {
        let writing = Arc::default();
        let first = Writing::start(&writing, Kind::Volume, "v");
        let second = Writing::start(&writing, Kind::Volume, "v");
        drop(first);
        assert!(
            writing
                .lock()
                .unwrap()
                .contains_key(&(Kind::Volume, "v".to_owned()))
        );
        drop(second);
        assert!(writing.lock().unwrap().is_empty());
    }

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
    fn a_sandbox_made_again_is_named_by_an_id_and_capabilities_alone() {
        for id in ["sbx_4f2a9c", "1a2b3c4d", "a-b_c"] {
            assert!(id_like(id), "{id}");
        }
        for id in ["", "../x", "a/b", "_x", "-x", "a b", &"a".repeat(65)] {
            assert!(!id_like(id), "{id}");
        }
        assert!(word(&capability_for(PortProtocol::Tcp), 128));
        assert_eq!(capability_for(PortProtocol::Http).len(), 32);
        assert!(!word("ab-cd", 128));
        assert!(!word(&"a".repeat(129), 128));
    }
}
