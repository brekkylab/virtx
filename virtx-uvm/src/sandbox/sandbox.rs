use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::ffi::OsString;
use std::hash::{BuildHasher as _, Hasher as _, RandomState};
use std::io;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::net::TcpStream;
use tokio::sync::oneshot;
use virtx_core::{ImageName, SandboxCreateArgs, SandboxJob, SandboxJobEntry, SandboxJobKind};

use super::{Error, Job};
use crate::machine::wire::CtrlResp;
use crate::{
    Digest, ImageHold, ImageStore, ImageStoreError, Machine, MachineConfig, MachineSpec, Network,
    Ports, Reference, SnapshotError, SnapshotStore, VolumeMount, VolumeStore,
};

/// A machine's size where a sandbox leaves it to the server.
pub const DEFAULT_VCPUS: u8 = 4;
pub const DEFAULT_MEMORY_MIB: u32 = 4096;

/// How many entries a job's log keeps before its end.
pub const LOG_LIMIT: usize = 100_000;

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

/// A sandbox: the machine it booted, up until the sandbox is closed, and the processes run in
/// it.
pub struct Sandbox {
    /// The image it was made from, as the store names it, and held there for as long as the
    /// sandbox is.
    image: Digest,
    _held: ImageHold,
    snapshot: Option<String>,
    machine: Arc<Machine>,
    /// Whether its machine runs still.
    up: AtomicBool,
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
    /// another sandbox stands on a snapshot, which this one is then refused.
    pub async fn create(
        images: &ImageStore,
        snapshots: &SnapshotStore,
        volumes: &VolumeStore,
        held: impl Fn(&str) -> bool,
        args: &SandboxCreateArgs,
        vm: Vec<OsString>,
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
            && (snapshot.in_use()? || held(&snapshot.name))
        {
            return Err(SnapshotError::InUse(snapshot.name.clone()).into());
        }
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

        // Merging the layers' metadata is blocking work.
        let store = images.clone();
        let rootfs = tokio::task::spawn_blocking(move || store.insert_rootfs(&layers)).await??;

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

        // Booting is blocking work, until the guest takes commands.
        let machine = tokio::task::spawn_blocking(move || Machine::start(config, &vm)).await??;
        let sandbox = Arc::new(Sandbox {
            image: digest,
            _held: held_image,
            snapshot: args.snapshot.clone(),
            machine: Arc::new(machine),
            up: AtomicBool::new(true),
            jobs: Mutex::default(),
            starting: Mutex::default(),
            ports: Ports::default(),
            used: Mutex::new(Instant::now()),
            active: AtomicUsize::new(0),
        });
        let reading = Arc::clone(&sandbox);
        std::thread::spawn(move || reading.read());
        Ok(sandbox)
    }

    /// The sandbox as the server answers it under `id`, with no ports: only its making
    /// answers those.
    pub fn answer(&self, id: String) -> virtx_core::Sandbox {
        virtx_core::Sandbox {
            id,
            image: self.image.to_string(),
            snapshot: self.snapshot.clone(),
            running: self.up.load(Ordering::Relaxed),
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
        let (waiter, started) = oneshot::channel();
        {
            let mut starting = self.starting.lock().unwrap();
            if !self.up.load(Ordering::Relaxed) {
                return Err(Error::Ended);
            }
            self.machine.exec(cmd.clone())?;
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
                let (pid, running) = job.pid();
                if let Some(sandbox) = sandbox.upgrade()
                    && running
                {
                    let _ = sandbox.machine.signal(pid, SIGKILL);
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
        let (pid, running) = job.pid();
        if running {
            self.machine.signal(pid, SIGKILL)?;
        }
        job.remove();
        Ok(())
    }

    /// Send the job `id` `signal`, as [`signal_number`] numbers it.
    pub fn signal(&self, id: &str, signal: i32) -> Result<(), Error> {
        let (pid, running) = self.job(id)?.pid();
        if !running {
            return Err(Error::JobEnded("the job has ended"));
        }
        self.machine.signal(pid, signal)?;
        Ok(())
    }

    /// Refused for every job: none is started with a stdin to write to.
    pub fn stdin(&self, id: &str) -> Result<(), Error> {
        self.job(id)?;
        Err(Error::JobEnded("the job was started without stdin"))
    }

    /// Its machine, to ask of its files.
    pub fn machine(&self) -> Result<Arc<Machine>, Error> {
        match self.up.load(Ordering::Relaxed) {
            true => Ok(Arc::clone(&self.machine)),
            false => Err(Error::Ended),
        }
    }

    /// A connection to `port` in its machine. Refused when the machine has ended, as a port
    /// nothing listens on is.
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

    /// Stop its machine, and end and remove every job in it, as cancelling each does. Blocks
    /// while the guest puts what it wrote on its snapshot.
    pub fn close(&self) {
        self.machine.stop();
        for job in std::mem::take(&mut *self.jobs.lock().unwrap()).into_values() {
            job.end(128 + SIGKILL);
            job.remove();
        }
    }

    /// What its machine says, until it ends: each command's pid, as the job its waiter gets,
    /// and each job's output and end.
    fn read(self: Arc<Self>) {
        let _published = self.ports.publish(self.machine.published.clone());
        let mut running = HashMap::new();
        while let Some(message) = self.machine.recv() {
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
        // Gone, and what ran in it with it: killed, as far as anything in it can tell.
        let mut starting = self.starting.lock().unwrap();
        self.up.store(false, Ordering::Relaxed);
        starting.clear();
        for job in running.into_values() {
            job.end(128 + SIGKILL);
        }
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

/// A name for what a client named nothing: eight hex digits for which `taken` is false.
fn pick(taken: impl Fn(&str) -> bool) -> String {
    loop {
        let name = format!("{:08x}", RandomState::new().build_hasher().finish() as u32);
        if !taken(&name) {
            return name;
        }
    }
}
