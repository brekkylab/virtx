//! The RPC methods `virtx-core` names, each answered from the [`Home`] of the project its
//! request reaches, which [`access`](crate::access) puts in its extensions.

use std::collections::{HashMap, HashSet};
use std::fmt::Display;
use std::future::Future;
use std::hash::{BuildHasher as _, Hasher as _, RandomState};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::net::TcpStream;
use virtx_core::*;
use virtx_uvm::{
    BuildError, Digest, ImageConfiguration, ImageHold, ImageStore, ImageStoreError, Machine,
    MachineError, MachineReport, MachineSpec, Network, Os, Ports, Reference, RegistryClient,
    RegistryError, Snapshot, SnapshotError, SnapshotStore, VolumeError, VolumeMount, VolumeStore,
    build_step,
};

/// The codes a method refuses with: JSON-RPC's own, and this server's beside them.
const INVALID_PARAMS: i64 = -32602;
const INTERNAL_ERROR: i64 = -32603;
const NOT_FOUND: i64 = -31001;
const ALREADY_EXISTS: i64 = -31002;
const IN_USE: i64 = -31003;
const UNSUPPORTED: i64 = -31004;
const BOOT_FAILED: i64 = -31005;
const BUILD_FAILED: i64 = -31006;
const TIMED_OUT: i64 = -31007;
const REGISTRY_FAILED: i64 = -31008;

/// A machine's size where `sandbox.init` leaves it to the server.
const DEFAULT_VCPUS: u8 = 4;
const DEFAULT_MEMORY_MIB: u32 = 4096;

/// How old what nothing needs has to be before `image.prune` takes it: a pull, push or build
/// in flight has put it there and not yet pointed a reference at it.
pub const PRUNE_GRACE: Duration = Duration::from_secs(60 * 60);

/// What one project's requests reach: the stores, and the sandboxes its members made from
/// them, which are theirs together.
pub struct Home {
    images: ImageStore,
    pub snapshots: SnapshotStore,
    volumes: VolumeStore,
    /// This binary, which each machine is spawned as.
    exe: PathBuf,
    /// The sandboxes, by id, from `sandbox.init` to `sandbox.close` or until they idle.
    sandboxes: Mutex<HashMap<String, Arc<Sandbox>>>,
}

impl Home {
    /// The images in `images`, shared with every other project, and the snapshots and volumes
    /// under `root`.
    pub fn new(images: ImageStore, root: &Path) -> anyhow::Result<Home> {
        Ok(Home {
            images,
            snapshots: SnapshotStore::new(root)?,
            volumes: VolumeStore::new(root)?,
            exe: std::env::current_exe()?,
            sandboxes: Mutex::default(),
        })
    }

    /// Forget every sandbox that has run nothing, and been reached at none of its ports, for
    /// `idle`.
    pub fn forget_idle(&self, idle: Duration) {
        self.sandboxes.lock().unwrap().retain(|_, sandbox| {
            sandbox.running.load(Ordering::Relaxed) > 0
                || sandbox.tunnels.load(Ordering::Relaxed) > 0
                || sandbox.used.lock().unwrap().elapsed() < idle
        });
    }

    /// A tunnel to the port `capability` opens, in whichever of this home's sandboxes it is
    /// one of.
    pub fn tunnel(&self, capability: &str) -> Option<Tunnel> {
        let sandboxes = self.sandboxes.lock().unwrap();
        sandboxes.values().find_map(|sandbox| {
            let port = *sandbox.capabilities.get(capability)?;
            sandbox.tunnels.fetch_add(1, Ordering::Relaxed);
            Some(Tunnel {
                sandbox: Arc::clone(sandbox),
                port,
            })
        })
    }
}

/// A connection through one of a sandbox's ports, counted until it drops: a sandbox with one
/// is in use, whatever its commands do.
pub struct Tunnel {
    sandbox: Arc<Sandbox>,
    port: u16,
}

impl Tunnel {
    /// The port, in whichever of the sandbox's machines runs.
    pub async fn connect(&self) -> io::Result<TcpStream> {
        self.sandbox.ports.connect(self.port).await
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        *self.sandbox.used.lock().unwrap() = Instant::now();
        self.sandbox.tunnels.fetch_sub(1, Ordering::Relaxed);
    }
}

/// A sandbox: the machine each of its commands boots, which ends with the command.
struct Sandbox {
    /// The image it was made from, as the store names it, and held there for as long as the
    /// sandbox is.
    image: Digest,
    _held: ImageHold,
    machine: Machine,
    /// How many of its commands run now, and when the last one ended.
    running: AtomicUsize,
    used: Mutex<Instant>,
    /// Where its machines' ports are reached, where it answers each was opened, and the
    /// capability in each answer, by the port it opens.
    ports: Ports,
    opened: Vec<SandboxPort>,
    capabilities: HashMap<String, u16>,
    /// How many connections through its ports are open now.
    tunnels: AtomicUsize,
}

/// A command running in a sandbox, counted until it drops.
struct Running<'a>(&'a Sandbox);

impl Drop for Running<'_> {
    fn drop(&mut self) {
        *self.0.used.lock().unwrap() = Instant::now();
        self.0.running.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Every method, each answered from the [`Home`] in its request's extensions.
pub fn methods() -> Methods {
    let mut methods = Methods::new();
    add::<ImagePull, _, _>(&mut methods, image_pull);
    add::<ImagePush, _, _>(&mut methods, image_push);
    add::<ImageBuild, _, _>(&mut methods, image_build);
    add::<ImagePrune, _, _>(&mut methods, image_prune);
    add::<SnapshotList, _, _>(&mut methods, snapshot_list);
    add::<SnapshotCreate, _, _>(&mut methods, snapshot_create);
    add::<SnapshotRemove, _, _>(&mut methods, snapshot_remove);
    add::<VolumeList, _, _>(&mut methods, volume_list);
    add::<VolumeCreate, _, _>(&mut methods, volume_create);
    add::<VolumeRemove, _, _>(&mut methods, volume_remove);
    add::<VolumeMountList, _, _>(&mut methods, volume_mount_list);
    add::<VolumeMountCreate, _, _>(&mut methods, volume_mount_create);
    add::<VolumeMountRemove, _, _>(&mut methods, volume_mount_remove);
    add::<SandboxList, _, _>(&mut methods, sandbox_list);
    add::<SandboxInit, _, _>(&mut methods, sandbox_init);
    add::<SandboxExec, _, _>(&mut methods, sandbox_exec);
    add::<SandboxClose, _, _>(&mut methods, sandbox_close);
    methods
}

/// Answer `R` with `handler`, given the request's home, its args, and where its progress goes.
fn add<R, F, Fut>(methods: &mut Methods, handler: F)
where
    R: Request,
    R::Args: Send + 'static,
    R::Output: Send,
    F: Fn(Arc<Home>, R::Args, Notifier) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<R::Output, Refused>> + Send + 'static,
{
    methods.add::<R, _, _>(move |args, extensions| {
        let home = extensions.get::<Arc<Home>>().cloned();
        let notifier = extensions.get::<Notifier>().cloned();
        let answered = handler(
            home.expect("access lets no request through without one"),
            args,
            notifier.expect("every request has one"),
        );
        async move { answered.await.map_err(|refused| refused.0) }
    });
}

async fn image_pull(
    home: Arc<Home>,
    args: ImagePullArgs,
    notifier: Notifier,
) -> Result<ImagePullOutput, Refused> {
    let reference: Reference = args
        .reference
        .parse()
        .map_err(|e| refused(INVALID_PARAMS, format!("{}: {e}", args.reference)))?;
    let mut progress = 0;
    let report = |line: String| {
        progress += 1;
        notifier.notify::<ImagePull>(Progress {
            progress,
            total: None,
            content: Some(line),
        });
    };

    let registry = RegistryClient::new(home.images.clone(), args.auth);
    let (digest, image) = registry.pull(&reference, report).await?;
    home.images.insert_tag(&reference, &image)?;
    Ok(ImagePullOutput {
        digest: digest.to_string(),
    })
}

async fn image_push(
    home: Arc<Home>,
    args: ImagePushArgs,
    notifier: Notifier,
) -> Result<ImagePushOutput, Refused> {
    let reference: Reference = args
        .reference
        .parse()
        .map_err(|e| refused(INVALID_PARAMS, format!("{}: {e}", args.reference)))?;
    let mut progress = 0;
    let report = |line: String| {
        progress += 1;
        notifier.notify::<ImagePush>(Progress {
            progress,
            total: None,
            content: Some(line),
        });
    };

    let registry = RegistryClient::new(home.images.clone(), args.auth);
    let digest = registry.push(&reference, report).await?;
    Ok(ImagePushOutput {
        digest: digest.to_string(),
    })
}

async fn image_build(
    home: Arc<Home>,
    args: ImageBuildArgs,
    notifier: Notifier,
) -> Result<ImageBuildOutput, Refused> {
    let reference: Reference = args
        .reference
        .parse()
        .map_err(|e| refused(INVALID_PARAMS, format!("{}: {e}", args.reference)))?;
    let mut progress = 0;
    let mut report = |line: String| {
        progress += 1;
        notifier.notify::<ImageBuild>(Progress {
            progress,
            total: None,
            content: Some(line),
        });
    };

    let (mut image, mut layers, _base) = base(&home.images, &args.recipe.base, &mut report).await?;
    let steps = args.recipe.steps.len();
    for (index, step) in args.recipe.steps.iter().enumerate() {
        report(format!("STEP {}/{steps}: {step}", index + 1));
        build_step(
            &home.images,
            &mut image,
            &mut layers,
            step,
            &home.exe,
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
    let digest = home.images.insert_image(&image, &layers)?;
    home.images.insert_tag(&reference, &digest)?;
    Ok(ImageBuildOutput {
        digest: digest.to_string(),
    })
}

/// A build's base: its config and layers, and its image held for as long as the build runs.
/// Nothing for `scratch`, as in a Dockerfile; else the image tagged `base` here, or the one
/// pulled into the store now, anonymously, and left untagged: a build tags only what it builds.
async fn base(
    images: &ImageStore,
    base: &str,
    report: &mut (impl FnMut(String) + Send),
) -> Result<(ImageConfiguration, Vec<Digest>, Option<ImageHold>), Refused> {
    if base == "scratch" {
        let mut image = ImageConfiguration::default();
        image.set_os(Os::Linux);
        return Ok((image, Vec::new(), None));
    }
    let reference: Reference = base
        .parse()
        .map_err(|e| refused(INVALID_PARAMS, format!("base {base}: {e}")))?;
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

/// Delete what no reference and no sandbox needs, once it is too old to be what a pull, push
/// or build in flight is putting together.
async fn image_prune(
    home: Arc<Home>,
    _: ImagePruneArgs,
    _: Notifier,
) -> Result<ImagePruneOutput, Refused> {
    let images = home.images.clone();
    let pruned = tokio::task::spawn_blocking(move || images.prune(PRUNE_GRACE)).await??;
    Ok(ImagePruneOutput {
        images: pruned.images.iter().map(Digest::to_string).collect(),
        reclaimed: pruned.reclaimed,
    })
}

async fn snapshot_list(
    home: Arc<Home>,
    _: SnapshotListArgs,
    _: Notifier,
) -> Result<Vec<SnapshotListOutputItem>, Refused> {
    let mut listed = Vec::new();
    for snapshot in home.snapshots.list()? {
        listed.push(SnapshotListOutputItem {
            size: snapshot.size()?,
            in_use: snapshot.in_use()?,
            name: snapshot.name,
        });
    }
    Ok(listed)
}

async fn snapshot_create(
    home: Arc<Home>,
    args: SnapshotCreateArgs,
    _: Notifier,
) -> Result<SnapshotCreateOutput, Refused> {
    if args.disk_gib == Some(0) {
        return Err(refused(
            INVALID_PARAMS,
            "disk_gib: 0 is a disk nothing fits on",
        ));
    }
    let size = args
        .disk_gib
        .map_or(Snapshot::SIZE, |gib| u64::from(gib) << 30);
    // Formatting a disk is blocking work.
    let snapshot = tokio::task::spawn_blocking(move || {
        let name = args
            .name
            .unwrap_or_else(|| pick(|name| home.snapshots.open(name).is_ok()));
        home.snapshots.create(&name, size)
    })
    .await??;
    Ok(SnapshotCreateOutput {
        name: snapshot.name,
    })
}

async fn snapshot_remove(
    home: Arc<Home>,
    args: SnapshotRemoveArgs,
    _: Notifier,
) -> Result<SnapshotRemoveOutput, Refused> {
    home.snapshots.remove(&args.name)?;
    Ok(SnapshotRemoveOutput {})
}

async fn volume_list(
    home: Arc<Home>,
    _: VolumeListArgs,
    _: Notifier,
) -> Result<Vec<VolumeListOutputItem>, Refused> {
    let mut listed = Vec::new();
    for name in home.volumes.list()? {
        let volume = home.volumes.open(&name)?;
        listed.push(VolumeListOutputItem {
            size: volume.size(),
            name,
        });
    }
    Ok(listed)
}

async fn volume_create(
    home: Arc<Home>,
    args: VolumeCreateArgs,
    _: Notifier,
) -> Result<VolumeCreateOutput, Refused> {
    let name = args
        .name
        .unwrap_or_else(|| pick(|name| home.volumes.open(name).is_ok()));
    let volume = home.volumes.create(&name)?;
    Ok(VolumeCreateOutput {
        name: volume.name().to_owned(),
    })
}

async fn volume_remove(
    home: Arc<Home>,
    args: VolumeRemoveArgs,
    _: Notifier,
) -> Result<VolumeRemoveOutput, Refused> {
    home.volumes.remove(&args.name)?;
    Ok(VolumeRemoveOutput {})
}

async fn volume_mount_list(
    home: Arc<Home>,
    args: VolumeMountListArgs,
    _: Notifier,
) -> Result<Vec<VolumeMountListOutputItem>, Refused> {
    let volume = home.volumes.open(&args.volume)?;
    Ok(volume
        .mounts()
        .iter()
        .map(|mount| VolumeMountListOutputItem {
            path: mount.path.clone(),
            mount_type: mount.source.mount_type(),
        })
        .collect())
}

async fn volume_mount_create(
    home: Arc<Home>,
    args: VolumeMountCreateArgs,
    _: Notifier,
) -> Result<VolumeMountCreateOutput, Refused> {
    let mut volume = home.volumes.open(&args.volume)?;
    volume.mount(args.path, args.source)?;
    Ok(VolumeMountCreateOutput {})
}

async fn volume_mount_remove(
    home: Arc<Home>,
    args: VolumeMountRemoveArgs,
    _: Notifier,
) -> Result<VolumeMountRemoveOutput, Refused> {
    let mut volume = home.volumes.open(&args.volume)?;
    volume.unmount(&args.path)?;
    Ok(VolumeMountRemoveOutput {})
}

async fn sandbox_list(
    home: Arc<Home>,
    _: SandboxListArgs,
    _: Notifier,
) -> Result<Vec<SandboxListOutputItem>, Refused> {
    let sandboxes = home.sandboxes.lock().unwrap();
    Ok(sandboxes
        .iter()
        .map(|(id, sandbox)| SandboxListOutputItem {
            id: id.clone(),
            image: sandbox.image.to_string(),
            snapshot: sandbox
                .machine
                .snapshot
                .as_ref()
                .map(|snapshot| snapshot.name.clone()),
            running: sandbox.running.load(Ordering::Relaxed) > 0,
            ports: sandbox.opened.clone(),
        })
        .collect())
}

async fn sandbox_init(
    home: Arc<Home>,
    args: SandboxInitArgs,
    _: Notifier,
) -> Result<SandboxInitOutput, Refused> {
    // What can be refused without starting anything, first.
    let mut given = HashSet::new();
    if let Some(port) = args
        .ports
        .iter()
        .find(|port| **port == 0 || !given.insert(**port))
    {
        return Err(refused(
            INVALID_PARAMS,
            format!("ports: {port} is not a port, or is given twice"),
        ));
    }
    for (what, given) in [
        ("vcpus", args.vcpus.map(u32::from)),
        ("memory_mib", args.memory_mib),
        ("gpu_memory_mib", args.gpu_memory_mib),
    ] {
        if given == Some(0) {
            return Err(refused(
                INVALID_PARAMS,
                format!("{what}: 0 is a machine nothing runs on"),
            ));
        }
    }
    let gpu = args.gpu.unwrap_or(false);
    if gpu && !cfg!(feature = "gpu") {
        return Err(refused(UNSUPPORTED, "this server was built without a GPU"));
    }
    if args.gpu_memory_mib.is_some() && !gpu {
        return Err(refused(
            INVALID_PARAMS,
            "gpu_memory_mib: a size is of a GPU, and this sandbox has none",
        ));
    }

    let digest = match &args.image {
        ImageName::Tag { tag } => {
            let reference: Reference = tag
                .parse()
                .map_err(|e| refused(INVALID_PARAMS, format!("{tag}: {e}")))?;
            home.images
                .get_tag(&reference)?
                .ok_or_else(|| refused(NOT_FOUND, format!("no image here is tagged {tag}")))?
        }
        ImageName::Digest { digest } => {
            let digest = digest
                .parse::<Digest>()
                .map_err(|e| refused(INVALID_PARAMS, format!("{digest}: {e}")))?;
            // A registry's digest, as a pull or a push answers, names the image it copied.
            home.images.get_image_origin(&digest)?.unwrap_or(digest)
        }
    };
    let missing = || refused(NOT_FOUND, format!("no image here is {digest}"));
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
        && snapshot.in_use()?
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

    let machine = Machine {
        rootfs,
        config: image.config().clone().unwrap_or_default(),
        snapshot,
        spec: MachineSpec {
            vcpus: args.vcpus.unwrap_or(DEFAULT_VCPUS),
            ram_mib: args.memory_mib.unwrap_or(DEFAULT_MEMORY_MIB),
            gpu,
            gpu_memory_mib: args.gpu_memory_mib,
            // A server others reach: what this host reaches is not a sandbox's to reach.
            network: match args.network {
                true => Network::Public,
                false => Network::None,
            },
        },
        volumes,
        ports: args.ports.clone(),
    };

    // Each port behind a capability of its own, which only this answer carries: who has the
    // URL reaches the port, and nothing else. Relative, to the URL this request came to, since
    // what reaches this server from outside is not this server's to know.
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

    // One sandbox here to a snapshot, as one machine at a time is to it: two sandboxes' commands
    // would otherwise take turns refusing each other.
    let mut sandboxes = home.sandboxes.lock().unwrap();
    if let Some(snapshot) = &machine.snapshot
        && sandboxes.values().any(|sandbox| {
            sandbox
                .machine
                .snapshot
                .as_ref()
                .is_some_and(|theirs| theirs.name == snapshot.name)
        })
    {
        return Err(SnapshotError::InUse(snapshot.name.clone()).into());
    }
    let id = pick(|id| sandboxes.contains_key(id));
    sandboxes.insert(
        id.clone(),
        Arc::new(Sandbox {
            image: digest,
            _held: held,
            machine,
            running: AtomicUsize::new(0),
            used: Mutex::new(Instant::now()),
            ports: Ports::default(),
            opened: opened.clone(),
            capabilities,
            tunnels: AtomicUsize::new(0),
        }),
    );
    Ok(SandboxInitOutput { id, ports: opened })
}

async fn sandbox_exec(
    home: Arc<Home>,
    args: SandboxExecArgs,
    notifier: Notifier,
) -> Result<SandboxExecOutput, Refused> {
    if args.cmd.is_empty() {
        return Err(refused(INVALID_PARAMS, "cmd: an exec names a command"));
    }
    let sandbox = home.sandboxes.lock().unwrap().get(&args.sandbox).cloned();
    let sandbox = sandbox.ok_or_else(|| {
        refused(
            NOT_FOUND,
            format!("no sandbox here has the id {}", args.sandbox),
        )
    })?;
    sandbox.running.fetch_add(1, Ordering::Relaxed);
    let _running = Running(&sandbox);
    let mut progress = 0;
    // Where the sandbox's ports reach, for as long as this machine runs.
    let mut _published = None;
    let code = sandbox
        .machine
        .clone()
        .run(
            &home.exe,
            args.cmd,
            args.timeout_ms,
            |report| match report {
                // The command's output, stdout and stderr alike; the rest is the machine's own.
                MachineReport::Stdout(line) | MachineReport::Stderr(line) => {
                    progress += 1;
                    notifier.notify::<SandboxExec>(Progress {
                        progress,
                        total: None,
                        content: Some(line),
                    });
                }
                MachineReport::Published(addresses) => {
                    _published = Some(sandbox.ports.publish(addresses));
                }
                MachineReport::Console(_) | MachineReport::Log { .. } => {}
            },
        )
        .await?;
    Ok(SandboxExecOutput { code })
}

async fn sandbox_close(
    home: Arc<Home>,
    args: SandboxCloseArgs,
    _: Notifier,
) -> Result<SandboxCloseOutput, Refused> {
    home.sandboxes
        .lock()
        .unwrap()
        .remove(&args.sandbox)
        .ok_or_else(|| {
            refused(
                NOT_FOUND,
                format!("no sandbox here has the id {}", args.sandbox),
            )
        })?;
    Ok(SandboxCloseOutput {})
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

/// A method's refusal: an error response, made from whatever failed.
struct Refused(ErrorObject);

fn refused(code: i64, message: impl Display) -> Refused {
    Refused(ErrorObject::new(code, message.to_string()))
}

/// The code an operation on the host's files that failed with `e` is refused with.
fn io_code(e: &std::io::Error) -> i64 {
    match e.kind() {
        std::io::ErrorKind::NotFound => NOT_FOUND,
        std::io::ErrorKind::AlreadyExists => ALREADY_EXISTS,
        _ => INTERNAL_ERROR,
    }
}

impl From<SnapshotError> for Refused {
    fn from(e: SnapshotError) -> Refused {
        let code = match &e {
            SnapshotError::InvalidName(_) => INVALID_PARAMS,
            SnapshotError::Unknown(_) => NOT_FOUND,
            SnapshotError::AlreadyExists(_) => ALREADY_EXISTS,
            SnapshotError::InUse(_) => IN_USE,
            SnapshotError::Io { source, .. } => io_code(source),
            SnapshotError::Fstree(_) => INTERNAL_ERROR,
        };
        refused(code, e)
    }
}

impl From<VolumeError> for Refused {
    fn from(e: VolumeError) -> Refused {
        let code = match &e {
            VolumeError::InvalidName(_)
            | VolumeError::InvalidPath(_)
            | VolumeError::Refused(_)
            | VolumeError::Source(_) => INVALID_PARAMS,
            VolumeError::Unknown(_) | VolumeError::NoMount { .. } => NOT_FOUND,
            VolumeError::AlreadyExists(_) => ALREADY_EXISTS,
            VolumeError::InUse(_) => IN_USE,
            VolumeError::Io { source, .. } => io_code(source),
            VolumeError::Origins(..) => INTERNAL_ERROR,
        };
        refused(code, e)
    }
}

impl From<ImageStoreError> for Refused {
    fn from(e: ImageStoreError) -> Refused {
        let code = match &e {
            ImageStoreError::Missing(_) => NOT_FOUND,
            ImageStoreError::InUse(..) => IN_USE,
            ImageStoreError::Io { source, .. } => io_code(source),
            ImageStoreError::Fstree(_) | ImageStoreError::Malformed(_) => INTERNAL_ERROR,
        };
        refused(code, e)
    }
}

/// What a build's step fails with: its command, or what it is made of.
impl From<BuildError> for Refused {
    fn from(e: BuildError) -> Refused {
        match e {
            BuildError::Failed { .. } => refused(BUILD_FAILED, e),
            BuildError::Store(e) => e.into(),
            BuildError::Machine(e) => e.into(),
            BuildError::Snapshot(e) => e.into(),
            BuildError::Fstree(_) | BuildError::Io { .. } => refused(INTERNAL_ERROR, e),
        }
    }
}

/// What a copy to or from another registry fails with.
impl From<RegistryError> for Refused {
    fn from(e: RegistryError) -> Refused {
        match e {
            RegistryError::Untagged(_) => refused(NOT_FOUND, e),
            RegistryError::Store(e) => e.into(),
            RegistryError::Fstree(_) | RegistryError::Io { .. } => refused(INTERNAL_ERROR, e),
            RegistryError::Unauthorized { .. }
            | RegistryError::Registry { .. }
            | RegistryError::Manifest(..) => refused(REGISTRY_FAILED, e),
        }
    }
}

/// What a sandbox's commands fail with.
impl From<MachineError> for Refused {
    fn from(e: MachineError) -> Refused {
        let code = match &e {
            MachineError::NoCommand => INVALID_PARAMS,
            MachineError::DidNotStart(_) => BOOT_FAILED,
            MachineError::TimedOut(_) => TIMED_OUT,
            _ => INTERNAL_ERROR,
        };
        refused(code, e)
    }
}

impl From<tokio::task::JoinError> for Refused {
    fn from(e: tokio::task::JoinError) -> Refused {
        refused(INTERNAL_ERROR, e)
    }
}
