use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::ffi::OsString;
use std::fs::{self, File};
use std::hash::{BuildHasher as _, Hasher as _, RandomState};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::net::TcpStream;
use tokio::sync::oneshot;
use virtx_core::{ImageName, SandboxCreateArgs, SandboxJob, SandboxJobEntry, SandboxJobKind};

use super::{Error, Job};
use crate::machine::wire::CtrlResp;
use crate::{
    Digest, FsTree, ImageHold, ImageStore, ImageStoreError, Machine, MachineConfig, MachineSpec,
    Network, Ports, Reference, Snapshot, SnapshotError, SnapshotLock, SnapshotStore, VolumeMount,
    VolumeStore,
};

/// A machine's size where a sandbox leaves it to the server.
pub const DEFAULT_VCPUS: u8 = 4;
pub const DEFAULT_MEMORY_MIB: u32 = 4096;

/// How many entries a job's log keeps before its end.
pub const LOG_LIMIT: usize = 100_000;

/// How long a sandbox is in use by nothing before its machine is stopped, where
/// `VIRTX_PARK_AFTER_MS` does not say: in milliseconds, `0` for never.
const PARK_AFTER: Duration = Duration::from_secs(5 * 60);

/// How long a sandbox is in use by nothing before its machine is stopped, `None` for never:
/// `VIRTX_PARK_AFTER_MS`, in milliseconds, or five minutes.
pub fn park_after() -> Option<Duration> {
    match std::env::var("VIRTX_PARK_AFTER_MS").map(|ms| ms.parse::<u64>()) {
        Ok(Ok(0)) => None,
        Ok(Ok(ms)) => Some(Duration::from_millis(ms)),
        _ => Some(PARK_AFTER),
    }
}

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

/// The signal `name`, such as `SIGTERM`, as Linux numbers it.
pub fn signal_number(name: &str) -> Option<i32> {
    let index = SIGNALS.iter().position(|signal| *signal == name)?;
    Some(index as i32 + 1)
}

/// A command sent to the machine and not yet answered, and who waits for its job.
type Starting = (Vec<String>, oneshot::Sender<Result<Arc<Job>, String>>);

/// Where a sandbox's machine is.
enum State {
    Up(Arc<Machine>),
    /// Stopped while the sandbox was in use by nothing, and booted again for what next needs
    /// it.
    Parked,
    /// Ended on its own, or with the sandbox: nothing boots it again.
    Exited,
}

/// A sandbox: a machine booted as it was made, again whenever it is needed after it was
/// [parked](Sandbox::idle) for want of use, and the processes run in it.
///
/// What it wrote is kept across its machine's stops, on its snapshot or on the one it made for
/// itself; what ran in it is not.
pub struct Sandbox {
    /// The image it was made from, as the store names it, and held there for as long as the
    /// sandbox is.
    image: Digest,
    _held: ImageHold,
    /// The snapshot it was made on, by name.
    snapshot: Option<String>,
    /// The one it made for itself, without one, until it is closed.
    upper: Mutex<Option<Upper>>,
    /// Its snapshot's lock, held for each of its machines and between them, until it is
    /// closed.
    snapshot_lock: Mutex<Option<SnapshotLock>>,
    /// What each of its machines boots, and the process each runs as.
    config: MachineConfig,
    vm: Vec<OsString>,
    state: Mutex<State>,
    /// Held while its machine boots or stops, so one does at a time.
    turn: tokio::sync::Mutex<()>,
    /// Its jobs, by id, until each is removed.
    jobs: Mutex<BTreeMap<String, Arc<Job>>>,
    /// Each command sent to the machine and not yet answered, in the order sent, and who waits
    /// for its job.
    starting: Mutex<VecDeque<Starting>>,
    /// Where its machine's ports are reached.
    ports: Ports,
    /// When it was made, or a job in it or an [`Activity`] on it last ended.
    used: Mutex<Instant>,
    /// How many [`Activity`]s are under way.
    active: AtomicUsize,
}

impl Sandbox {
    /// Boot a sandbox as `args` asks, of what `images`, `snapshots` and `volumes` hold, by
    /// running `vm` as its machine's process (see [`Machine::start`]). `held` says whether
    /// another sandbox stands on a snapshot, which this one is then refused. Its machine is
    /// stopped once the sandbox has been in use by nothing for `park_after`.
    pub async fn create(
        images: &ImageStore,
        snapshots: &SnapshotStore,
        volumes: &VolumeStore,
        held: impl Fn(&str) -> bool,
        args: &SandboxCreateArgs,
        vm: Vec<OsString>,
        park_after: Option<Duration>,
    ) -> Result<Arc<Sandbox>, Error> {
        // What can be refused without starting anything, first.
        let mut given = HashSet::new();
        if let Some(port) = args
            .ports
            .iter()
            .find(|port| **port == 0 || !given.insert(**port))
        {
            return Err(Error::Invalid(format!(
                "ports: {port} is not a port, or is given twice"
            )));
        }
        for (what, given) in [
            ("vcpus", args.vcpus.map(u32::from)),
            ("memory_mib", args.memory_mib),
            ("gpu_memory_mib", args.gpu_memory_mib),
        ] {
            if given == Some(0) {
                return Err(Error::Invalid(format!(
                    "{what}: 0 is a machine nothing runs on"
                )));
            }
        }
        let gpu = args.gpu.unwrap_or(false);
        if gpu && !cfg!(feature = "gpu") {
            return Err(Error::Unsupported(
                "this server was built without a GPU".into(),
            ));
        }
        if args.gpu_memory_mib.is_some() && !gpu {
            return Err(Error::Invalid(
                "gpu_memory_mib: a size is of a GPU, and this sandbox has none".into(),
            ));
        }

        let digest = match &args.image {
            ImageName::Tag { tag } => {
                let reference: Reference = tag
                    .parse()
                    .map_err(|e| Error::Invalid(format!("{tag}: {e}")))?;
                images
                    .get_tag(&reference)?
                    .ok_or_else(|| Error::NotFound(format!("no image here is tagged {tag}")))?
            }
            ImageName::Digest { digest } => {
                let digest = digest
                    .parse::<Digest>()
                    .map_err(|e| Error::Invalid(format!("{digest}: {e}")))?;
                // A registry's digest names the image pulled or pushed as it.
                images.get_image_origin(&digest)?.unwrap_or(digest)
            }
        };
        let missing = || Error::NotFound(format!("no image here is {digest}"));
        let held_image = images.hold_image(&digest).map_err(|e| match e {
            ImageStoreError::Missing(_) => missing(),
            e => e.into(),
        })?;
        let (image, layers) = images.get_image(&digest)?.ok_or_else(missing)?;

        let snapshot = args
            .snapshot
            .as_deref()
            .map(|name| snapshots.open(name))
            .transpose()?;
        if let Some(snapshot) = &snapshot
            && held(&snapshot.name)
        {
            return Err(SnapshotError::InUse(snapshot.name.clone()).into());
        }
        // Held from here until the sandbox is closed, whether its machine runs or not, against
        // every other sandbox, removal and request to its files, in this process or another.
        let snapshot_lock = snapshot.as_ref().map(Snapshot::lock).transpose()?;
        let volumes = args
            .volumes
            .iter()
            .map(|asked| {
                Ok(VolumeMount {
                    path: volumes.open(&asked.volume)?.path().to_owned(),
                    at: asked.path.clone(),
                    read_only: asked.readonly,
                })
            })
            .collect::<Result<_, Error>>()?;

        // Merging the layers' metadata, and formatting a snapshot, are blocking work.
        let store = images.clone();
        let rootfs = tokio::task::spawn_blocking(move || store.insert_rootfs(&layers)).await??;
        let (upper, snapshot_lock) = match snapshot_lock {
            Some(lock) => (None, lock),
            None => {
                let dir = Upper::dir(snapshots);
                let upper = tokio::task::spawn_blocking(move || Upper::create(&dir)).await??;
                let lock = upper.snapshot.lock()?;
                (Some(upper), lock)
            }
        };

        let config = MachineConfig {
            rootfs,
            config: image.config().clone().unwrap_or_default(),
            snapshot: snapshot.or_else(|| upper.as_ref().map(|upper| upper.snapshot.clone())),
            snapshot_held: true,
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
        let sandbox = Arc::new(Sandbox {
            image: digest,
            _held: held_image,
            snapshot: args.snapshot.clone(),
            upper: Mutex::new(upper),
            snapshot_lock: Mutex::new(Some(snapshot_lock)),
            config,
            vm,
            state: Mutex::new(State::Parked),
            turn: tokio::sync::Mutex::new(()),
            jobs: Mutex::default(),
            starting: Mutex::default(),
            ports: Ports::default(),
            used: Mutex::new(Instant::now()),
            active: AtomicUsize::new(0),
        });
        sandbox.wake().await?;
        if let Some(after) = park_after {
            tokio::spawn(park_when_idle(Arc::downgrade(&sandbox), after));
        }
        Ok(sandbox)
    }

    /// The sandbox as the server answers it under `id`, with no ports: only its making
    /// answers those.
    pub fn answer(&self, id: String) -> virtx_core::Sandbox {
        virtx_core::Sandbox {
            id,
            image: self.image.to_string(),
            snapshot: self.snapshot.clone(),
            running: !matches!(*self.state.lock().unwrap(), State::Exited),
            ports: Vec::new(),
        }
    }

    /// The snapshot it stands on, by name.
    pub fn snapshot(&self) -> Option<&str> {
        self.snapshot.as_deref()
    }

    /// Start `cmd` in it, beside whatever else runs there, and kill it after `timeout`.
    pub async fn exec(
        self: &Arc<Self>,
        cmd: Vec<String>,
        timeout: Option<Duration>,
    ) -> Result<Arc<Job>, Error> {
        // In use until the job runs, which keeps it in use from there.
        let _activity = self.activity();
        let machine = self.wake().await?;
        let (waiter, started) = oneshot::channel();
        {
            let mut starting = self.starting.lock().unwrap();
            if !self.is(&machine) {
                return Err(Error::Ended);
            }
            machine.exec(cmd.clone())?;
            starting.push_back((cmd, waiter));
        }
        let job = started
            .await
            .map_err(|_| Error::Ended)?
            .map_err(Error::NotStarted)?;
        if let Some(timeout) = timeout {
            let (sandbox, job) = (Arc::downgrade(self), Arc::clone(&job));
            tokio::spawn(async move {
                tokio::time::sleep(timeout).await;
                if let Some(sandbox) = sandbox.upgrade() {
                    let _ = sandbox.kill(&job);
                }
            });
        }
        Ok(job)
    }

    /// Its jobs as they are now, in the order of their ids.
    pub fn jobs(&self) -> Vec<SandboxJob> {
        let jobs = self.jobs.lock().unwrap();
        jobs.values().map(|job| job.get()).collect()
    }

    pub fn job(&self, id: &str) -> Result<Arc<Job>, Error> {
        let jobs = self.jobs.lock().unwrap();
        jobs.get(id)
            .cloned()
            .ok_or_else(|| Error::NotFound(format!("no job here has the id {id}")))
    }

    /// Kill the job `id` if it runs, and remove it.
    pub fn remove_job(&self, id: &str) -> Result<(), Error> {
        let removed = self.jobs.lock().unwrap().remove(id);
        let job = removed.ok_or_else(|| Error::NotFound(format!("no job here has the id {id}")))?;
        self.kill(&job)?;
        job.remove();
        Ok(())
    }

    /// Send the job `id` `signal`, as [`signal_number`] numbers it.
    pub fn signal(&self, id: &str, signal: i32) -> Result<(), Error> {
        let (pid, running) = self.job(id)?.pid();
        // A job that runs keeps the machine it runs in up.
        let machine = self.up().filter(|_| running);
        let machine = machine.ok_or(Error::JobEnded("the job has ended"))?;
        machine.signal(pid, signal)?;
        Ok(())
    }

    /// Refused for every job: none is started with a stdin to write to.
    pub fn stdin(&self, id: &str) -> Result<(), Error> {
        self.job(id)?;
        Err(Error::JobEnded("the job was started without stdin"))
    }

    /// Its machine, booted again if it was parked, to ask of its files. The caller holds an
    /// [`Activity`] for as long as it asks, so the machine is not parked under it.
    pub async fn machine(self: &Arc<Self>) -> Result<Arc<Machine>, Error> {
        self.wake().await
    }

    /// A connection to `port` in its machine. Refused while no machine runs, as a port nothing
    /// listens on is: a parked one is not booted for it, since nothing would listen there.
    pub async fn connect(&self, port: u16) -> io::Result<TcpStream> {
        self.ports.connect(port).await
    }

    /// Something done with it from outside, such as a connection through a port or a request
    /// to its files: the sandbox is in use until it drops.
    pub fn activity(self: &Arc<Self>) -> Activity {
        self.active.fetch_add(1, Ordering::Relaxed);
        Activity(Arc::clone(self))
    }

    /// How long it has been in use by nothing: no job running and no [`Activity`] under way.
    /// `None` while it is in use.
    pub fn idle(&self) -> Option<Duration> {
        let running = self.jobs.lock().unwrap().values().any(|job| job.pid().1);
        if running || self.active.load(Ordering::Relaxed) > 0 {
            return None;
        }
        Some(self.used.lock().unwrap().elapsed())
    }

    /// Stop its machine for good, and end and remove every job in it, as cancelling each does.
    /// Blocks while the guest puts what it wrote on its snapshot.
    pub fn close(&self) {
        let state = std::mem::replace(&mut *self.state.lock().unwrap(), State::Exited);
        if let State::Up(machine) = state {
            machine.stop();
        }
        drop(self.snapshot_lock.lock().unwrap().take());
        drop(self.upper.lock().unwrap().take());
        for job in std::mem::take(&mut *self.jobs.lock().unwrap()).into_values() {
            job.end(128 + SIGKILL);
            job.remove();
        }
    }

    /// Its machine, booting one if it is parked.
    async fn wake(self: &Arc<Self>) -> Result<Arc<Machine>, Error> {
        let _turn = self.turn.lock().await;
        match &*self.state.lock().unwrap() {
            State::Up(machine) => return Ok(Arc::clone(machine)),
            State::Exited => return Err(Error::Ended),
            State::Parked => {}
        }
        // Booting is blocking work, until the guest takes commands.
        let (config, vm) = (self.config.clone(), self.vm.clone());
        let machine = tokio::task::spawn_blocking(move || Machine::start(config, &vm)).await??;
        let machine = Arc::new(machine);
        let closed = {
            let mut state = self.state.lock().unwrap();
            let closed = matches!(*state, State::Exited);
            if !closed {
                *state = State::Up(Arc::clone(&machine));
            }
            closed
        };
        // Closed while it booted: what booted goes with it.
        if closed {
            tokio::task::spawn_blocking(move || machine.stop()).await?;
            return Err(Error::Ended);
        }
        let (reading, read) = (Arc::clone(self), Arc::clone(&machine));
        std::thread::spawn(move || reading.read(&read));
        Ok(machine)
    }

    /// Stop its machine if it has been in use by nothing for `after`, to boot again when it
    /// is next needed.
    async fn park(&self, after: Duration) {
        let _turn = self.turn.lock().await;
        if self.idle().is_none_or(|idle| idle < after) {
            return;
        }
        let machine = {
            let mut state = self.state.lock().unwrap();
            let State::Up(machine) = &*state else {
                return;
            };
            let machine = Arc::clone(machine);
            *state = State::Parked;
            machine
        };
        let _ = tokio::task::spawn_blocking(move || machine.stop()).await;
    }

    /// Its machine while one runs.
    fn up(&self) -> Option<Arc<Machine>> {
        match &*self.state.lock().unwrap() {
            State::Up(machine) => Some(Arc::clone(machine)),
            _ => None,
        }
    }

    /// Whether `machine` is its machine now.
    fn is(&self, machine: &Arc<Machine>) -> bool {
        matches!(&*self.state.lock().unwrap(), State::Up(up) if Arc::ptr_eq(up, machine))
    }

    /// Kill `job` if it runs.
    fn kill(&self, job: &Job) -> Result<(), Error> {
        let (pid, running) = job.pid();
        if let Some(machine) = self.up().filter(|_| running) {
            machine.signal(pid, SIGKILL)?;
        }
        Ok(())
    }

    /// What `machine` says, until it ends: each command's pid, as the job its waiter gets, and
    /// each job's output and end.
    fn read(&self, machine: &Arc<Machine>) {
        let _published = self.ports.publish(machine.published.clone());
        let mut running = HashMap::new();
        while let Some(message) = machine.recv() {
            match message {
                CtrlResp::Started(started) => {
                    let Some((cmd, waiter)) = self.starting.lock().unwrap().pop_front() else {
                        continue;
                    };
                    let started = started.map(|pid| {
                        let mut jobs = self.jobs.lock().unwrap();
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
                CtrlResp::Stdout { id, line } => {
                    if let Some(job) = running.get(&id) {
                        job.write(SandboxJobEntry::Stdout(line));
                    }
                }
                CtrlResp::Stderr { id, line } => {
                    if let Some(job) = running.get(&id) {
                        job.write(SandboxJobEntry::Stderr(line));
                    }
                }
                CtrlResp::Exited { id, code } => {
                    if let Some(job) = running.remove(&id) {
                        job.end(code);
                        *self.used.lock().unwrap() = Instant::now();
                    }
                }
                CtrlResp::Ready(_) | CtrlResp::Log { .. } => {}
            }
        }
        // Gone, and what ran in it with it: killed, as far as anything in it can tell. Parked,
        // nothing ran; otherwise it ended on its own, and nothing boots it again.
        let mut starting = self.starting.lock().unwrap();
        {
            let mut state = self.state.lock().unwrap();
            if matches!(&*state, State::Up(up) if Arc::ptr_eq(up, machine)) {
                *state = State::Exited;
            }
        }
        starting.clear();
        for job in running.into_values() {
            job.end(128 + SIGKILL);
        }
    }
}

/// Park `sandbox`'s machine each time it has been in use by nothing for `after`, until the
/// sandbox goes or its machine has exited.
async fn park_when_idle(sandbox: std::sync::Weak<Sandbox>, after: Duration) {
    let mut wait = after;
    loop {
        tokio::time::sleep(wait).await;
        let Some(sandbox) = sandbox.upgrade() else {
            return;
        };
        if matches!(*sandbox.state.lock().unwrap(), State::Exited) {
            return;
        }
        wait = match sandbox.idle() {
            Some(idle) if idle >= after => {
                sandbox.park(after).await;
                after
            }
            Some(idle) => after - idle,
            None => after,
        };
    }
}

/// Something done with a sandbox from outside, counted until it drops: a sandbox with one is
/// in use, whatever its jobs do.
pub struct Activity(Arc<Sandbox>);

impl Activity {
    pub fn sandbox(&self) -> &Arc<Sandbox> {
        &self.0
    }
}

impl Drop for Activity {
    fn drop(&mut self) {
        *self.0.used.lock().unwrap() = Instant::now();
        self.0.active.fetch_sub(1, Ordering::Relaxed);
    }
}

/// The snapshot a sandbox made without one makes for itself, deleted when the sandbox is
/// closed, or by the next [`Sandbox::sweep`] once the process that made it has ended.
///
/// Each is `<name>.ext4` in `<root>/sandbox/`, beside `<name>.owner`, which the process that
/// made it holds a lock on for as long as it runs.
struct Upper {
    snapshot: Snapshot,
    owner: PathBuf,
    _held: File,
}

impl Upper {
    fn dir(snapshots: &SnapshotStore) -> PathBuf {
        snapshots.root().join("sandbox")
    }

    fn create(dir: &Path) -> Result<Upper, Error> {
        let io = |context: String| move |source| SnapshotError::Io { context, source };
        fs::create_dir_all(dir).map_err(io(format!("making {}", dir.display())))?;
        let name = pick(|name| dir.join(format!("{name}.owner")).exists());
        let owner = dir.join(format!("{name}.owner"));
        let held = File::create_new(&owner).map_err(io(format!("creating {}", owner.display())))?;
        held.try_lock().map_err(|e| SnapshotError::Io {
            context: format!("locking {}", owner.display()),
            source: e.into(),
        })?;
        let path = dir.join(format!("{name}.ext4"));
        // Owned from here, so a disk that failed to format is deleted too.
        let upper = Upper {
            snapshot: Snapshot { name, path },
            owner,
            _held: held,
        };
        let file = File::create_new(&upper.snapshot.path)
            .map_err(io(format!("creating {}", upper.snapshot.path.display())))?;
        FsTree::new()
            .to_ext4(file, Snapshot::SIZE)
            .map_err(SnapshotError::from)?;
        Ok(upper)
    }
}

impl Drop for Upper {
    fn drop(&mut self) {
        self.snapshot.delete();
        let _ = fs::remove_file(&self.owner);
    }
}

impl Sandbox {
    /// Delete what sandboxes made under `snapshots`' root by processes that have since ended
    /// left: a server that was killed closed none of its own.
    pub fn sweep(snapshots: &SnapshotStore) {
        let dir = Upper::dir(snapshots);
        let Ok(entries) = fs::read_dir(&dir) else {
            return;
        };
        for entry in entries.flatten() {
            let owner = entry.path();
            if owner.extension().is_none_or(|e| e != "owner") {
                continue;
            }
            let Ok(file) = File::open(&owner) else {
                continue;
            };
            if file.try_lock().is_err() {
                continue;
            }
            let Some(name) = owner.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            let path = dir.join(format!("{name}.ext4"));
            Snapshot {
                name: name.to_owned(),
                path,
            }
            .delete();
            let _ = fs::remove_file(&owner);
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
