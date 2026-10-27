//! The RPC methods `virtx-core` names, answered from this host's [`Home`]: one client is on the
//! other end of stdio, so all of it is its.

use std::collections::HashMap;
use std::fmt::Display;
use std::future::Future;
use std::hash::{BuildHasher as _, Hasher as _, RandomState};
use std::path::Path;
use std::sync::{Arc, Mutex};

use virtx_core::*;
use virtx_uvm::{
    Digest, ImageStore, ImageStoreError, Machine, MachineConfig, MachineError, Network, Reference,
    Snapshot, SnapshotError, SnapshotStore, VolumeError, VolumeMount, VolumeStore,
};

/// The codes a method refuses with: JSON-RPC's own, and this server's beside them.
const INVALID_PARAMS: i64 = -32602;
const INTERNAL_ERROR: i64 = -32603;
const NOT_FOUND: i64 = -31001;
const ALREADY_EXISTS: i64 = -31002;
const IN_USE: i64 = -31003;
const UNSUPPORTED: i64 = -31004;
const BOOT_FAILED: i64 = -31005;
const NOT_STARTED: i64 = -31006;
const TIMED_OUT: i64 = -31007;

/// A machine's size where `sandbox.init` leaves it to the server.
const DEFAULT_VCPUS: u8 = 4;
const DEFAULT_MEMORY_MIB: u32 = 4096;

/// What the methods answer from: the stores, and the sandboxes started from them.
pub struct Home {
    images: ImageStore,
    snapshots: SnapshotStore,
    volumes: VolumeStore,
    /// The sandboxes, by id, from `sandbox.init` to `sandbox.close`.
    machines: Mutex<HashMap<String, Arc<Machine>>>,
}

impl Home {
    /// Everything under `root`: `images/`, `snapshot/` and `volume/`.
    pub fn new(root: &Path) -> anyhow::Result<Home> {
        Ok(Home {
            images: ImageStore::new(root.join("images"))?,
            snapshots: SnapshotStore::new(root)?,
            volumes: VolumeStore::new(root)?,
            machines: Mutex::default(),
        })
    }

    /// Close every sandbox, as the server ends.
    pub async fn close_all(&self) {
        let closing: Vec<_> = self.machines.lock().unwrap().drain().collect();
        for (id, machine) in closing {
            if let Err(e) = machine.stop().await {
                eprintln!("closing sandbox {id}: {e}");
            }
        }
    }
}

/// Every method, each answered from `home`.
pub fn methods(home: &Arc<Home>) -> Methods {
    let mut methods = Methods::new();
    add::<ImageBuild, _, _>(&mut methods, home, image_build);
    add::<SnapshotList, _, _>(&mut methods, home, snapshot_list);
    add::<SnapshotCreate, _, _>(&mut methods, home, snapshot_create);
    add::<SnapshotRemove, _, _>(&mut methods, home, snapshot_remove);
    add::<VolumeList, _, _>(&mut methods, home, volume_list);
    add::<VolumeCreate, _, _>(&mut methods, home, volume_create);
    add::<VolumeRemove, _, _>(&mut methods, home, volume_remove);
    add::<VolumeMountList, _, _>(&mut methods, home, volume_mount_list);
    add::<VolumeMountCreate, _, _>(&mut methods, home, volume_mount_create);
    add::<VolumeMountRemove, _, _>(&mut methods, home, volume_mount_remove);
    add::<SandboxList, _, _>(&mut methods, home, sandbox_list);
    add::<SandboxInit, _, _>(&mut methods, home, sandbox_init);
    add::<SandboxExec, _, _>(&mut methods, home, sandbox_exec);
    add::<SandboxClose, _, _>(&mut methods, home, sandbox_close);
    methods
}

/// Answer `R` with `handler`, given the home, the request's args, and where its progress goes.
fn add<R, F, Fut>(methods: &mut Methods, home: &Arc<Home>, handler: F)
where
    R: Request,
    R::Args: Send + 'static,
    R::Output: Send,
    F: Fn(Arc<Home>, R::Args, Notifier) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<R::Output, Refused>> + Send + 'static,
{
    let home = Arc::clone(home);
    methods.add::<R, _, _>(move |args, extensions| {
        let notifier = extensions.get::<Notifier>().cloned();
        let answered = handler(
            Arc::clone(&home),
            args,
            notifier.expect("every request has one"),
        );
        async move { answered.await.map_err(|refused| refused.0) }
    });
}

async fn image_build(
    _: Arc<Home>,
    _: ImageBuildArgs,
    _: Notifier,
) -> Result<ImageBuildOutput, Refused> {
    // TODO: run the recipe's steps in a machine and file what they made.
    Err(refused(UNSUPPORTED, "image.build is not implemented yet"))
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
    let machines = home.machines.lock().unwrap();
    Ok(machines
        .iter()
        .map(|(id, machine)| SandboxListOutputItem {
            id: id.clone(),
            image: machine.config().image.to_string(),
            snapshot: machine
                .config()
                .snapshot
                .as_ref()
                .map(|snapshot| snapshot.name.clone()),
            running: machine.running(),
            daemon: false,
            ports: Vec::new(),
        })
        .collect())
}

async fn sandbox_init(
    home: Arc<Home>,
    args: SandboxInitArgs,
    _: Notifier,
) -> Result<SandboxInitOutput, Refused> {
    // What can be refused without starting anything, first.
    if args.daemon {
        return Err(refused(
            UNSUPPORTED,
            "a sandbox over stdio ends with its client, so none is a daemon",
        ));
    }
    if !args.ports.is_empty() {
        return Err(refused(INVALID_PARAMS, "ports are only for a daemon"));
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
        ImageName::Digest { digest } => digest
            .parse::<Digest>()
            .map_err(|e| refused(INVALID_PARAMS, format!("{digest}: {e}")))?,
    };
    let (image, layers) = home
        .images
        .get_image(&digest)?
        .ok_or_else(|| refused(NOT_FOUND, format!("no image here is {digest}")))?;

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

    let machine = Machine::start(MachineConfig {
        image: digest,
        rootfs,
        config: image.config().clone().unwrap_or_default(),
        snapshot,
        vcpus: args.vcpus.unwrap_or(DEFAULT_VCPUS),
        ram_mib: args.memory_mib.unwrap_or(DEFAULT_MEMORY_MIB),
        gpu,
        gpu_memory_mib: args.gpu_memory_mib,
        volumes,
        // A server over stdio is on its client's own computer.
        network: match args.network {
            true => Network::Host,
            false => Network::None,
        },
        ports: Vec::new(),
    })
    .await
    .map_err(|e| refused(BOOT_FAILED, e))?;

    let mut machines = home.machines.lock().unwrap();
    let id = pick(|id| machines.contains_key(id));
    machines.insert(id.clone(), Arc::new(machine));
    Ok(SandboxInitOutput {
        id,
        ports: Vec::new(),
    })
}

async fn sandbox_exec(
    home: Arc<Home>,
    args: SandboxExecArgs,
    notifier: Notifier,
) -> Result<SandboxExecOutput, Refused> {
    if args.cmd.is_empty() {
        return Err(refused(INVALID_PARAMS, "cmd: an exec names a command"));
    }
    let machine = home.machines.lock().unwrap().get(&args.sandbox).cloned();
    let machine = machine.ok_or_else(|| {
        refused(
            NOT_FOUND,
            format!("no sandbox here has the id {}", args.sandbox),
        )
    })?;
    let mut progress = 0;
    let code = machine
        .exec(args.cmd, args.timeout_ms, |line| {
            progress += 1;
            notifier.notify::<SandboxExec>(Progress {
                progress,
                total: None,
                content: Some(line),
            });
        })
        .await?;
    Ok(SandboxExecOutput { code })
}

async fn sandbox_close(
    home: Arc<Home>,
    args: SandboxCloseArgs,
    _: Notifier,
) -> Result<SandboxCloseOutput, Refused> {
    let machine = home.machines.lock().unwrap().remove(&args.sandbox);
    let machine = machine.ok_or_else(|| {
        refused(
            NOT_FOUND,
            format!("no sandbox here has the id {}", args.sandbox),
        )
    })?;
    machine.stop().await?;
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
            ImageStoreError::Fstree(_) => INTERNAL_ERROR,
        };
        refused(code, e)
    }
}

/// What a running machine's commands fail with.
impl From<MachineError> for Refused {
    fn from(e: MachineError) -> Refused {
        let code = match &e {
            MachineError::NotStarted(_) => NOT_STARTED,
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
