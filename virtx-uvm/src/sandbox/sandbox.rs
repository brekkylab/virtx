use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::ffi::OsString;
use std::fs::{self, File};
use std::hash::{BuildHasher as _, Hasher as _, RandomState};
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, UNIX_EPOCH};

use sha2::{Digest as _, Sha256};

use tokio::net::TcpStream;
use tokio::sync::oneshot;
use virtx_core::{
    ImageName, SandboxCreateArgs, SandboxJob, SandboxJobEndReason, SandboxJobEntry, SandboxJobKind,
};

use super::record::Record;
use super::upper::Upper;
use super::{Error, Job};
use crate::machine::wire::CtrlResp;
use crate::{
    Digest, ImageHold, ImageStore, ImageStoreError, Machine, MachineConfig, MachineSpec, Network,
    Ports, Reference, VolumeHold, VolumeMount, VolumeStore,
};

/// A machine's size where a sandbox leaves it to the server.
pub const DEFAULT_VCPUS: u8 = 4;
pub const DEFAULT_MEMORY_MIB: u32 = 4096;
pub const DEFAULT_DISK_GIB: u32 = 2;
/// The least memory a sandbox is given: the kernel alone takes about 21 MiB.
const MIN_MEMORY_MIB: u32 = 64;

/// The most `disk_gib` a sandbox may ask for. Formatting a disk keeps 64 KiB of its bitmaps in
/// memory for each GiB.
const MAX_DISK_GIB: u32 = 1024;

/// The directory under a server's root its sandboxes keep what they make on the host in.
const SANDBOX: &str = "sandbox";

/// The directory beside the server's program its own programs for `/abin` are in, and where each
/// one's `SKILL.md` is in that.
const ABIN_HOST: &str = "abin";
const SKILLS: &str = "skills";
const SKILL: &str = "SKILL.md";

/// How many entries a job's log keeps before its end.
pub const LOG_LIMIT: usize = 100_000;

/// How many ended jobs a sandbox keeps, each with its log, for a client to read: past them, the
/// one that ended first is removed as another starts, as if its client had removed it. One that
/// runs is never removed so.
pub const KEEP_ENDED: usize = 100;

/// Remove from `jobs` the ones that ended first, leaving room for one more under `keep` ended.
fn keep_ended(jobs: &mut BTreeMap<String, Arc<Job>>, keep: usize) {
    let mut ended: Vec<(Instant, String)> = jobs
        .iter()
        .filter_map(|(id, job)| job.ended_at().map(|at| (at, id.clone())))
        .collect();
    if ended.len() < keep {
        return;
    }
    ended.sort();
    for (_, id) in &ended[..ended.len() + 1 - keep] {
        if let Some(job) = jobs.remove(id) {
            job.remove();
        }
    }
}

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
pub(super) const SIGKILL: i32 = 9;

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
/// What it wrote is kept across its machine's stops, on the upper disk it made for itself, until
/// it is closed; what ran in it is not. One made with keys is kept past its process too, with a
/// record beside its disk: [let go](Sandbox::leave) of as its server ends, it is known again,
/// parked, by the next server over its root ([`Sandbox::kept`]).
pub struct Sandbox {
    /// Its upper disk's name, and so its own.
    id: String,
    /// Where its upper disk and its record are.
    dir: PathBuf,
    /// What is kept of it past its process, for one made so: `None` for one that goes with its
    /// process, and once it is closed.
    record: Mutex<Option<Record>>,
    /// The image it was made from, as the store names it, and held there for as long as the
    /// sandbox is.
    image: Digest,
    _held: ImageHold,
    /// The upper disk it made for itself, until it is closed.
    upper: Mutex<Option<Upper>>,
    /// The programs it was made with, by name, and the directory its machines show them from,
    /// until it is closed.
    abins: Vec<String>,
    abin: Mutex<Option<Abin>>,
    /// Its volumes, held against removal for each of its machines and between them, until it
    /// is closed.
    volume_holds: Mutex<Vec<VolumeHold>>,
    /// What each of its machines boots, and the process each runs as.
    config: MachineConfig,
    vm: Vec<OsString>,
    state: Mutex<State>,
    /// Why a job that runs as it ends is ended, once it is closed or let go of.
    ended_for: Mutex<Option<SandboxJobEndReason>>,
    /// Held while its machine boots or stops, so one does at a time.
    turn: tokio::sync::Mutex<()>,
    /// Its jobs, by id, until each is removed.
    jobs: Mutex<BTreeMap<String, Arc<Job>>>,
    /// Each command sent to the machine and not yet answered, in the order sent, and who waits
    /// for its job.
    starting: Mutex<VecDeque<Starting>>,
    /// Where its machine's ports are reached.
    ports: Ports,
    /// How long it is in use by nothing before its machine is parked, `None` for never, and
    /// whether that is watched for, from its first boot on.
    park_after: Option<Duration>,
    watching: AtomicBool,
    /// When it was made, or a job in it or an [`Activity`] on it last ended.
    used: Mutex<Instant>,
    /// How many [`Activity`]s are under way.
    active: AtomicUsize,
    /// What a conditional write to its files holds as it lands (see [`Files`](crate::Files)).
    writing: Arc<tokio::sync::Mutex<()>>,
}

impl Sandbox {
    /// Boot a sandbox as `args` asks, of what `images` and `volumes` hold, writing over its
    /// image to an upper disk it makes under `root`, by running `vm` as its machine's process
    /// (see [`Machine::start`]). Its machine is stopped once the sandbox has been in use by
    /// nothing for `park_after`.
    ///
    /// With `keys`, the port each opens, it is kept past this process: its record is written
    /// once it has booted, and a making cut short before then leaves nothing. Without, it goes
    /// with this process.
    pub async fn create(
        images: &ImageStore,
        root: &Path,
        volumes: &VolumeStore,
        args: &SandboxCreateArgs,
        vm: Vec<OsString>,
        park_after: Option<Duration>,
        keys: Option<HashMap<String, u16>>,
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
        let mut at = HashSet::new();
        for volume in &args.volumes {
            let why = match crate::mount_point(&volume.path) {
                Err(why) => why,
                Ok(path) => match at.insert(path) {
                    false => "another volume is there",
                    true => continue,
                },
            };
            return Err(Error::Invalid(format!(
                "volumes: {} at {}: {why}",
                volume.volume, volume.path
            )));
        }
        match args.disk_gib {
            Some(0) => {
                return Err(Error::Invalid(
                    "disk_gib: 0 is a disk nothing fits on".into(),
                ));
            }
            Some(gib) if gib > MAX_DISK_GIB => {
                return Err(Error::Invalid(format!(
                    "disk_gib: {gib} is over the {MAX_DISK_GIB} a sandbox may have"
                )));
            }
            _ => {}
        }
        for (what, given) in [
            ("vcpus", args.vcpus.map(u32::from)),
            ("gpu_memory_mib", args.gpu_memory_mib),
        ] {
            if given == Some(0) {
                return Err(Error::Invalid(format!(
                    "{what}: 0 is a machine nothing runs on"
                )));
            }
        }
        if let Some(mib) = args.memory_mib.filter(|mib| *mib < MIN_MEMORY_MIB) {
            return Err(Error::Invalid(format!(
                "memory_mib: {mib} is under the {MIN_MEMORY_MIB} a machine boots in"
            )));
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
        // The programs are those beside the one its machines run as, as `cargo xtask install`
        // and a release put them there.
        let programs = vm.first().and_then(|program| Path::new(program).parent());
        let programs = programs.map(|dir| dir.join(ABIN_HOST)).unwrap_or_default();
        // Every name for being given twice before any for being here, so that a list refused
        // for itself is refused whatever this server has.
        let mut given = HashSet::new();
        if let Some(name) = args.abins.iter().find(|name| !given.insert(*name)) {
            return Err(Error::Invalid(format!("abins: `{name}` is given twice")));
        }
        for name in &args.abins {
            let mut parts = Path::new(name).components();
            let plain = matches!(
                (parts.next(), parts.next()),
                (Some(Component::Normal(part)), None) if part == name.as_str()
            );
            if name.starts_with('.') || !plain || !programs.join(name).is_file() {
                return Err(Error::Invalid(format!(
                    "abins: this server has no program called `{name}`"
                )));
            }
        }

        let digest = match &args.image {
            ImageName::Tag { tag } => {
                let reference: Reference = tag
                    .parse()
                    .map_err(|e| Error::Invalid(format!("{tag}: {e}")))?;
                images.get_tag(&reference)?.ok_or_else(|| {
                    Error::NotFound(format!("no image here is tagged {tag}: pull it first"))
                })?
            }
            ImageName::Digest { digest } => {
                let digest = digest
                    .parse::<Digest>()
                    .map_err(|e| Error::Invalid(format!("{digest}: {e}")))?;
                // A registry's digest names the image pulled or pushed as it.
                images.get_image_origin(&digest)?.unwrap_or(digest)
            }
        };
        // What it boots as every time: the image by the digest it boots now.
        let mut args = args.clone();
        args.image = ImageName::Digest {
            digest: digest.to_string(),
        };
        let (images, root, volumes, made) = (
            images.clone(),
            root.to_owned(),
            volumes.clone(),
            args.clone(),
        );
        let built = move || {
            let given = (None, None);
            Sandbox::build(
                &images, &root, &volumes, &made, digest, vm, park_after, given,
            )
        };
        let sandbox = Arc::new(tokio::task::spawn_blocking(built).await??);
        sandbox.wake().await?;
        if let Some(keys) = keys {
            let record = Record {
                args,
                keys,
                exited: false,
                jobs: Vec::new(),
            };
            if let Err(e) = sandbox.keep(record) {
                let closed = Arc::clone(&sandbox);
                tokio::task::spawn_blocking(move || closed.close()).await?;
                return Err(e);
            }
        }
        Ok(sandbox)
    }

    /// The sandbox `args` makes, of image `digest`, parked: over the upper disk `kept` gives,
    /// where it gives one, and otherwise over one it makes; exited where the record `kept`
    /// gives says so. Blocking work: merging the image's layers' metadata, and formatting an
    /// upper disk.
    #[allow(clippy::too_many_arguments)]
    fn build(
        images: &ImageStore,
        root: &Path,
        volumes: &VolumeStore,
        args: &SandboxCreateArgs,
        digest: Digest,
        vm: Vec<OsString>,
        park_after: Option<Duration>,
        kept: (Option<Upper>, Option<Record>),
    ) -> Result<Sandbox, Error> {
        let (upper, record) = kept;
        let missing = || Error::NotFound(format!("no image here is {digest}"));
        let held_image = images.hold_image(&digest).map_err(|e| match e {
            ImageStoreError::Missing(_) => missing(),
            e => e.into(),
        })?;
        let (image, layers) = images.get_image(&digest)?.ok_or_else(missing)?;

        let mut volume_holds = Vec::new();
        let volumes = args
            .volumes
            .iter()
            .map(|asked| {
                let volume = volumes.open(&asked.volume)?;
                volume_holds.push(volume.hold()?);
                Ok(VolumeMount {
                    path: volume.path().to_owned(),
                    at: asked.path.clone(),
                    read_only: asked.readonly,
                })
            })
            .collect::<Result<_, Error>>()?;

        let rootfs = images.insert_rootfs(&layers)?;
        let gpu = args.gpu.unwrap_or(false);
        let dir = root.join(SANDBOX);
        let upper = match upper {
            Some(upper) => upper,
            None => {
                let gib = args.disk_gib.unwrap_or(DEFAULT_DISK_GIB);
                loop {
                    if let Some(upper) = Upper::create(&dir, &pick(|_| false), gib)? {
                        break upper;
                    }
                }
            }
        };
        let abin = match args.abins.is_empty() {
            true => None,
            false => {
                let programs = vm.first().and_then(|program| Path::new(program).parent());
                let programs = programs.map(|dir| dir.join(ABIN_HOST)).unwrap_or_default();
                Some(Abin::create(&Abin::dir(root), &programs, &args.abins)?)
            }
        };

        let config = MachineConfig {
            rootfs,
            config: image.config().clone().unwrap_or_default(),
            upper: upper.path.clone(),
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
            abin: abin.as_ref().map(|abin| abin.dir.clone()),
            ports: args.ports.clone(),
        };
        let exited = record.as_ref().is_some_and(|record| record.exited);
        let jobs = record.iter().flat_map(|record| record.jobs.iter().cloned());
        let jobs = jobs.map(|job| (job.id.clone(), Job::kept(job))).collect();
        Ok(Sandbox {
            id: upper.id.clone(),
            dir,
            record: Mutex::new(record),
            image: digest,
            _held: held_image,
            abins: args.abins.clone(),
            upper: Mutex::new(Some(upper)),
            abin: Mutex::new(abin),
            volume_holds: Mutex::new(volume_holds),
            config,
            vm,
            state: Mutex::new(match exited {
                true => State::Exited,
                false => State::Parked,
            }),
            ended_for: Mutex::new(None),
            turn: tokio::sync::Mutex::new(()),
            jobs: Mutex::new(jobs),
            starting: Mutex::default(),
            ports: Ports::default(),
            park_after,
            watching: AtomicBool::new(false),
            used: Mutex::new(Instant::now()),
            active: AtomicUsize::new(0),
            writing: Arc::default(),
        })
    }

    /// What it is named by, which no other sandbox over the same root has while it is there.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The sandbox as the server answers it, with no ports: only its making answers those.
    pub fn answer(&self) -> virtx_core::Sandbox {
        virtx_core::Sandbox {
            id: self.id.clone(),
            image: self.image.to_string(),
            running: !matches!(*self.state.lock().unwrap(), State::Exited),
            ports: Vec::new(),
            abins: self.abins.clone(),
            error: None,
        }
    }

    /// The port each of its keys opens, as it was made with them: none for one not kept past
    /// its process.
    pub fn keys(&self) -> HashMap<String, u16> {
        let record = self.record.lock().unwrap();
        record
            .as_ref()
            .map(|record| record.keys.clone())
            .unwrap_or_default()
    }

    /// Keep it past this process as `record` says: written beside its upper disk, which is
    /// left for the next process over its root from now on.
    fn keep(&self, record: Record) -> Result<(), Error> {
        let mut kept = self.record.lock().unwrap();
        let mut upper = self.upper.lock().unwrap();
        let upper = upper.as_mut().ok_or(Error::Ended)?;
        record
            .write(&self.dir, &self.id)
            .map_err(|source| Error::Io {
                context: format!("writing sandbox {}'s record", self.id),
                source,
            })?;
        upper.kept = true;
        *kept = Some(record);
        Ok(())
    }

    /// Write its jobs as they are now into its record, for one kept past its process: a server
    /// started again over its root knows each, and each that runs now as ended by the restart.
    fn keep_jobs(&self) {
        let jobs = self.jobs();
        if let Some(record) = &mut *self.record.lock().unwrap() {
            record.jobs = jobs;
            if let Err(e) = record.write(&self.dir, &self.id) {
                eprintln!("sandbox {}'s jobs not kept: {e}", self.id);
            }
        }
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
                    let _ = sandbox.kill(&job, SandboxJobEndReason::Timeout);
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

    /// Kill the job `id` if it runs, ending it as cancelled, and remove it: what reads its log
    /// reads on to that end.
    pub fn remove_job(&self, id: &str) -> Result<(), Error> {
        let removed = self.jobs.lock().unwrap().remove(id);
        let job = removed.ok_or_else(|| Error::NotFound(format!("no job here has the id {id}")))?;
        self.kill(&job, SandboxJobEndReason::Cancelled)?;
        job.cut(SandboxJobEndReason::Cancelled);
        job.remove();
        self.keep_jobs();
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

    /// What a conditional write to its files holds first as it lands (see
    /// [`Files`](crate::Files)).
    pub(crate) fn writing(&self) -> &Arc<tokio::sync::Mutex<()>> {
        &self.writing
    }

    /// Each volume it mounts writable: where in the guest, spelled as the guest is given it,
    /// and the volume's own directory.
    pub(crate) fn mounts(&self) -> Vec<(String, PathBuf)> {
        self.config
            .volumes
            .iter()
            .filter(|mount| !mount.read_only)
            .filter_map(|mount| Some((crate::mount_point(&mount.at).ok()?, mount.path.clone())))
            .collect()
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

    /// Stop its machine for good, end and remove every job in it, as cancelling each does, and
    /// delete its upper disk, with its record if it is kept. Blocks while the guest shuts down,
    /// and so must not be called on an async runtime's thread.
    pub fn close(&self) {
        self.end(false);
    }

    /// Let go of it as its server ends: its machine stopped as parking stops it, every job in
    /// it ended, and nothing booting it again in this process. One kept past its process is
    /// left with what it wrote for the next server over its root ([`Sandbox::kept`]), its jobs
    /// that ran ended `restarted`; one not kept is deleted, as [`close`](Sandbox::close)
    /// deletes it. Blocks as `close` does.
    pub fn leave(&self) {
        self.end(true);
    }

    /// Stop its machine for good, and end and remove every job in it; its upper disk deleted,
    /// or left where it is kept and `leaving`. A job that runs ends as `restarted` where the
    /// sandbox is kept past its server, and as `sandbox_deleted` where it is deleted.
    fn end(&self, leaving: bool) {
        let reason = match leaving && self.record.lock().unwrap().is_some() {
            true => SandboxJobEndReason::Restarted,
            false => SandboxJobEndReason::SandboxDeleted,
        };
        *self.ended_for.lock().unwrap() = Some(reason);
        let state = std::mem::replace(&mut *self.state.lock().unwrap(), State::Exited);
        // A park or a boot under way finishes first, so its machine is down by the time this
        // returns: one being parked has synced, and one booting is stopped by `wake`, which
        // finds the sandbox closed.
        let _turn = self.turn.blocking_lock();
        // Taken first, so that nothing its machine's stopping ends -- its exit, a job killed
        // with it -- is written into it: a job that runs now is kept as one that ran.
        let record = self.record.lock().unwrap().take();
        if let State::Up(machine) = state {
            machine.stop();
        }
        self.volume_holds.lock().unwrap().clear();
        if let Some(mut upper) = self.upper.lock().unwrap().take() {
            upper.kept = leaving && record.is_some();
        }
        drop(self.abin.lock().unwrap().take());
        for job in std::mem::take(&mut *self.jobs.lock().unwrap()).into_values() {
            job.cut(reason);
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
        if let Some(after) = self.park_after
            && !self.watching.swap(true, Ordering::Relaxed)
        {
            tokio::spawn(park_when_idle(Arc::downgrade(self), after));
        }
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

    /// Kill `job` if it runs, for `reason`.
    fn kill(&self, job: &Job, reason: SandboxJobEndReason) -> Result<(), Error> {
        let (pid, running) = job.pid();
        if let Some(machine) = self.up().filter(|_| running) {
            job.killing(reason);
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
                        keep_ended(&mut jobs, KEEP_ENDED);
                        let id = pick(|id| jobs.contains_key(id));
                        let job = Job::new(SandboxJob {
                            id: id.clone(),
                            kind: SandboxJobKind::Exec,
                            pid,
                            cmd,
                            running: true,
                            code: None,
                            reason: None,
                        });
                        jobs.insert(id, Arc::clone(&job));
                        running.insert(pid, Arc::clone(&job));
                        job
                    });
                    self.keep_jobs();
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
                        self.keep_jobs();
                    }
                }
                CtrlResp::Ready(_) | CtrlResp::Log { .. } | CtrlResp::Pong => {}
            }
        }
        // Gone, and what ran in it with it: killed, as far as anything in it can tell. Closed or
        // let go of, it was stopped under them, for the reason `end` gave; otherwise it ended on
        // its own, and nothing boots it again.
        let mut starting = self.starting.lock().unwrap();
        let exited = {
            let mut state = self.state.lock().unwrap();
            let exited = matches!(&*state, State::Up(up) if Arc::ptr_eq(up, machine));
            if exited {
                *state = State::Exited;
            }
            exited
        };
        // Kept so, past its process, before any job is seen to end: a server started again
        // knows it exited.
        if exited && let Some(record) = &mut *self.record.lock().unwrap() {
            record.exited = true;
            if let Err(e) = record.write(&self.dir, &self.id) {
                eprintln!("sandbox {}'s exit not kept: {e}", self.id);
            }
        }
        starting.clear();
        let reason = match exited {
            true => SandboxJobEndReason::VmExited,
            false => self
                .ended_for
                .lock()
                .unwrap()
                .unwrap_or(SandboxJobEndReason::SandboxDeleted),
        };
        for job in running.into_values() {
            job.cut(reason);
        }
        drop(starting);
        self.keep_jobs();
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

/// The directory a sandbox made with [`abins`](SandboxCreateArgs::abins) shows at `/abin`: each
/// program it named, and its `SKILL.md` under `skills/`, linked from the server's own.
///
/// One is shared, read-only, by every sandbox made with the same programs as they stand: `<key>/`
/// in `<root>/sandbox/abin/`, keyed by the programs' names, sizes and times, so that only the
/// first copies them where a link cannot be made. Each sandbox holds `<key>.lock` shared for as
/// long as it shows one, and [`Abin::sweep`] deletes those none holds.
struct Abin {
    dir: PathBuf,
    _held: File,
}

impl Abin {
    fn dir(root: &Path) -> PathBuf {
        root.join(SANDBOX).join(ABIN_HOST)
    }

    /// The one in `dir` of `names`, each a program in `programs`, and the `SKILL.md` of each
    /// that has one: made, when no sandbox has made it yet.
    fn create(dir: &Path, programs: &Path, names: &[String]) -> Result<Abin, Error> {
        let io = |context: String| move |source| Error::Io { context, source };
        let mut files = Vec::new();
        for program in names {
            files.push(PathBuf::from(program));
            let skill = Path::new(SKILLS).join(program).join(SKILL);
            if programs.join(&skill).is_file() {
                files.push(skill);
            }
        }
        files.sort();
        let mut key = Sha256::new();
        key.update(programs.as_os_str().as_encoded_bytes());
        for file in &files {
            let from = programs.join(file);
            let meta = fs::metadata(&from).map_err(io(format!("reading {}", from.display())))?;
            let time = meta.modified().ok();
            let time = time.and_then(|time| time.duration_since(UNIX_EPOCH).ok());
            key.update([0]);
            key.update(file.as_os_str().as_encoded_bytes());
            key.update(meta.len().to_le_bytes());
            key.update(time.unwrap_or_default().as_nanos().to_le_bytes());
        }
        let key = format!("{:x}", key.finalize())[..16].to_owned();

        fs::create_dir_all(dir).map_err(io(format!("making {}", dir.display())))?;
        let lock = dir.join(format!("{key}.lock"));
        let held = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock)
            .map_err(io(format!("opening {}", lock.display())))?;
        // Waited for: a sweep holds it alone only while it deletes.
        held.lock_shared()
            .map_err(io(format!("locking {}", lock.display())))?;
        let made = dir.join(&key);
        if !made.is_dir() {
            // Filled under a name of its own, so that no sandbox shows one half made.
            let name = pick(|name| dir.join(format!("{key}.{name}")).exists());
            let part = dir.join(format!("{key}.{name}"));
            let filled = files
                .iter()
                .try_for_each(|file| link(&programs.join(file), &part.join(file)))
                .and_then(|()| {
                    fs::rename(&part, &made).map_err(|source| Error::Io {
                        context: format!("placing {}", made.display()),
                        source,
                    })
                });
            // Placed by another sandbox meanwhile, where the rename failed for that.
            if filled.is_err() {
                let _ = fs::remove_dir_all(&part);
                if !made.is_dir() {
                    filled?;
                }
            }
        }
        Ok(Abin {
            dir: made,
            _held: held,
        })
    }

    /// Delete each one in `dir` no sandbox holds, with what was being filled for it.
    fn sweep(dir: &Path) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for lock in entries.flatten().map(|entry| entry.path()) {
            if lock.extension().is_none_or(|e| e != "lock") {
                continue;
            }
            let (Ok(file), Some(key)) = (File::open(&lock), lock.file_stem()) else {
                continue;
            };
            if file.try_lock().is_err() {
                continue;
            }
            let key = key.to_string_lossy();
            let Ok(entries) = fs::read_dir(dir) else {
                return;
            };
            for made in entries.flatten().map(|entry| entry.path()) {
                let name = made.file_name().unwrap_or_default().to_string_lossy();
                let ours =
                    name == *key || name.strip_prefix(&*key).is_some_and(|n| n.starts_with('.'));
                if ours && made.is_dir() {
                    let _ = fs::remove_dir_all(&made);
                }
            }
            // The lock itself stays: one waiting for it holds what it opened, not a new one.
        }
    }
}

/// `from` at `to`, making its directory: a hard link, or a copy where `to` cannot be one, such
/// as on another filesystem.
fn link(from: &Path, to: &Path) -> Result<(), Error> {
    let io = |context: String| move |source| Error::Io { context, source };
    let parent = to.parent().unwrap_or(to);
    fs::create_dir_all(parent).map_err(io(format!("making {}", parent.display())))?;
    if fs::hard_link(from, to).is_err() {
        fs::copy(from, to).map_err(io(format!("copying {}", from.display())))?;
    }
    Ok(())
}

impl Sandbox {
    /// Delete what sandboxes made under `root` by processes that have since ended left, but
    /// those kept past their process: a server that was killed closed none of its own.
    pub fn sweep(root: &Path) {
        Upper::sweep(&root.join(SANDBOX));
        Abin::sweep(&Abin::dir(root));
    }

    /// Each sandbox kept under `root` past the process that made it or last knew it, that no
    /// other process knows now, known again here: parked, or exited where its machine ended
    /// on its own, with its jobs, each that ran then ended `restarted`, and booted again as [`create`](Sandbox::create) says
    /// for the next that needs it. What sandboxes not kept left is deleted first, as
    /// [`sweep`](Sandbox::sweep) deletes it. One that cannot be known again -- its record not
    /// read, or its image, a volume or an abin of it gone -- is [`Unknown`], its disk and
    /// record held where they are until it is removed. Blocking work, as making one is.
    pub fn kept(
        images: &ImageStore,
        root: &Path,
        volumes: &VolumeStore,
        vm: Vec<OsString>,
        park_after: Option<Duration>,
    ) -> Kept {
        let dir = root.join(SANDBOX);
        Upper::sweep(&dir);
        let mut kept = Kept::default();
        for upper in Upper::kept(&dir) {
            let id = upper.id.clone();
            let read = Record::read(&dir, &id);
            let image = read
                .as_ref()
                .ok()
                .and_then(|record| match &record.args.image {
                    ImageName::Digest { digest } => Some(digest.clone()),
                    ImageName::Tag { .. } => None,
                });
            let known = read.and_then(|record| {
                let ImageName::Digest { digest } = &record.args.image else {
                    return Err("its record names its image by no digest".into());
                };
                let digest = digest
                    .parse::<Digest>()
                    .map_err(|e| format!("{digest}: {e}"))?;
                let (args, kept) = (record.args.clone(), (Some(upper), Some(record)));
                let vm = vm.clone();
                Sandbox::build(images, root, volumes, &args, digest, vm, park_after, kept)
                    .map_err(|e| e.to_string())
            });
            match known {
                Ok(sandbox) => kept.known.push(Arc::new(sandbox)),
                Err(why) => {
                    eprintln!(
                        "sandbox {id} under {} not known again: {why}",
                        dir.display()
                    );
                    // Let go as the sandbox not made was dropped, kept; held again here.
                    if let Some(upper) = Upper::kept_one(&dir, &id) {
                        kept.unknown.push(Unknown {
                            id,
                            image,
                            why,
                            upper,
                        });
                    }
                }
            }
        }
        Abin::sweep(&Abin::dir(root));
        kept
    }

    /// Whether a sandbox, of this process or another, lives under `root`: one holds an upper
    /// disk there from before its machine boots until it is closed, and one kept past its
    /// process lives there until it is closed.
    pub fn lives_under(root: &Path) -> bool {
        Upper::lives(&root.join(SANDBOX))
    }
}

/// What a server started again over a root knows of the sandboxes kept there
/// ([`Sandbox::kept`]).
#[derive(Default)]
pub struct Kept {
    /// Each known again, parked or exited.
    pub known: Vec<Arc<Sandbox>>,
    /// Each that could not be.
    pub unknown: Vec<Unknown>,
}

/// A sandbox kept under a root that a server started again over it could not know again: its
/// record not read, or its image, a volume or an abin of it gone. Its disk and record stay
/// held, and its root counted as lived in ([`Sandbox::lives_under`]), until it is
/// [removed](Unknown::remove); dropped, they are let go and kept for the next server.
#[derive(Debug)]
pub struct Unknown {
    id: String,
    image: Option<String>,
    why: String,
    upper: Upper,
}

impl Unknown {
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Why it could not be known again.
    pub fn why(&self) -> &str {
        &self.why
    }

    /// It as a sandbox is answered: its image as its record names it, empty where that is not
    /// read, not `running`, and why in `error`.
    pub fn answer(&self) -> virtx_core::Sandbox {
        virtx_core::Sandbox {
            id: self.id.clone(),
            image: self.image.clone().unwrap_or_default(),
            running: false,
            ports: Vec::new(),
            abins: Vec::new(),
            error: Some(format!(
                "not known again since its server restarted: {}",
                self.why
            )),
        }
    }

    /// Delete its disk and record. Blocking work.
    pub fn remove(mut self) {
        self.upper.kept = false;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ended_jobs_past_the_limit_go_the_first_ended_first_and_none_that_runs() {
        let job = |id: &str, ended: bool| {
            let job = Job::new(SandboxJob {
                id: id.into(),
                kind: SandboxJobKind::Exec,
                pid: 1,
                cmd: vec!["true".into()],
                running: true,
                code: None,
                reason: None,
            });
            if ended {
                job.end(0);
            }
            (id.to_owned(), job)
        };
        let mut jobs = BTreeMap::new();
        // Ended in the reverse of their ids' order, so that the order kept is the ends'.
        for i in (0..5).rev() {
            let (id, job) = job(&format!("e{i}"), true);
            jobs.insert(id, job);
            std::thread::sleep(Duration::from_millis(2));
        }
        let (id, running) = job("r", false);
        jobs.insert(id, running);
        // Room for one more under three ended: the three that ended first go.
        keep_ended(&mut jobs, 3);
        assert_eq!(jobs.keys().collect::<Vec<_>>(), ["e0", "e1", "r"]);
        keep_ended(&mut jobs, 10);
        assert_eq!(jobs.len(), 3, "under the limit, none goes");
    }

    #[test]
    fn an_abin_holds_the_programs_named_with_their_skills_shared_until_swept() {
        let host = tempfile::tempdir().unwrap();
        let programs = host.path().join(ABIN_HOST);
        fs::create_dir_all(programs.join(SKILLS).join("index")).unwrap();
        for program in ["index", "docread", "display"] {
            fs::write(programs.join(program), program).unwrap();
        }
        fs::write(programs.join(SKILLS).join("index").join(SKILL), "skill").unwrap();

        let dir = host.path().join("sandbox").join(ABIN_HOST);
        let names = ["index".to_string(), "display".to_string()];
        let abin = Abin::create(&dir, &programs, &names).unwrap();
        assert_eq!(fs::read(abin.dir.join("index")).unwrap(), b"index");
        assert_eq!(fs::read(abin.dir.join("display")).unwrap(), b"display");
        assert!(!abin.dir.join("docread").exists());
        let skills = abin.dir.join(SKILLS);
        assert_eq!(
            fs::read(skills.join("index").join(SKILL)).unwrap(),
            b"skill"
        );
        assert!(!skills.join("display").exists());

        let reversed = ["display".to_string(), "index".to_string()];
        let same = Abin::create(&dir, &programs, &reversed).unwrap();
        assert_eq!(same.dir, abin.dir, "the same programs share one");
        let other = Abin::create(&dir, &programs, &names[..1]).unwrap();
        assert_ne!(other.dir, abin.dir);

        let (made, other_made) = (abin.dir.clone(), other.dir.clone());
        drop((abin, other));
        Abin::sweep(&dir);
        assert!(made.is_dir(), "one a sandbox still holds stays");
        assert!(!other_made.exists());
        drop(same);
        Abin::sweep(&dir);
        assert!(!made.exists());
        assert!(programs.join("index").is_file(), "the server's own stay");
    }
}
