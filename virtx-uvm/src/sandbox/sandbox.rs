use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::ffi::OsString;
use std::hash::{BuildHasher as _, Hasher as _, RandomState};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};
use virtx_core::{
    PortProtocol, PortSpec, SandboxCreateArgs, SandboxJob, SandboxJobEntry, SandboxJobKind,
    SandboxPort,
};

use super::hooks::{Held, Hooks, Written};
use super::record::{Record, Resized};
use super::{Error, Job};
use crate::machine::wire::CtrlResp;
use crate::{
    ImageStore, Machine, MachineConfig, MachineSpec, Network, Ports, SnapshotError, SnapshotLock,
    SnapshotStore, VolumeMount, VolumeStore,
};

/// A machine's size where a sandbox leaves it to the server.
pub const DEFAULT_VCPUS: u8 = 4;
pub const DEFAULT_MEMORY_MIB: u32 = 4096;
/// The least memory a sandbox is given: the kernel alone takes about 21 MiB.
const MIN_MEMORY_MIB: u32 = 64;

/// How many entries a job's log keeps before its end.
pub const LOG_LIMIT: usize = 100_000;

/// How many ended jobs a sandbox keeps, each with its log, for a client to read: past them, the
/// one that ended first is removed as another starts, as if its client had removed it. One
/// running is never removed so.
pub const KEEP_ENDED: usize = 100;

/// `jobs`, with room for one more under [`KEEP_ENDED`]: the ones that ended first removed.
fn keep_ended(jobs: &mut BTreeMap<String, Arc<Job>>, keep: usize) {
    let mut ended: Vec<(std::time::Instant, String)> = jobs
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

/// The most a port's label is, in characters.
pub const LABEL_MOST: usize = 128;

/// How long a sandbox starting waits for its disk to be let go of: by what keeps it, as it
/// stopped.
const DISK_WAIT: Duration = Duration::from_secs(10);

/// How long a machine has to say where a port it was asked to publish is.
const PUBLISH_WAIT: Duration = Duration::from_secs(10);

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

/// The directory beside the program machines run as that virtx's own programs for `/abin` are
/// in, and where each one's `SKILL.md` is in that.
const ABIN_HOST: &str = "abin";
const SKILLS: &str = "skills";
const SKILL: &str = "SKILL.md";

/// What every sandbox of a server boots from and keeps itself in.
pub(super) struct Shared {
    pub(super) images: ImageStore,
    pub(super) snapshots: SnapshotStore,
    pub(super) volumes: VolumeStore,
    /// The process each machine runs as.
    pub(super) vm: Vec<OsString>,
    pub(super) hooks: Arc<dyn Hooks>,
    /// Where each record is kept, `<id>.json`.
    pub(super) kept: PathBuf,
}

/// Where a sandbox's machine is.
enum State {
    Running(Arc<Run>),
    Stopped,
    Deleted,
}

/// A command sent to a sandbox's machine, not yet answered: what it is, where its output goes
/// if it is on a terminal, and who waits for its job.
struct Starting {
    cmd: Vec<String>,
    terminal: Option<mpsc::UnboundedSender<String>>,
    waiter: oneshot::Sender<Result<Arc<Job>, String>>,
}

/// Who waits for a port the machine was asked to publish: where it is, or why not.
type Publishing = oneshot::Sender<Result<(u16, std::net::SocketAddr), String>>;

/// Who waits for a machine's answer about its disk at a mark, and the mark.
type Waiting = (u64, oneshot::Sender<Result<bool, String>>);

/// A sandbox's run: the machine it booted, up until it stops, and what is asked of it.
pub(super) struct Run {
    pub(super) machine: Arc<Machine>,
    /// Its disk's lock, and what the hooks hold for it, until it has stopped.
    held: Mutex<Option<(SnapshotLock, Vec<Held>)>>,
    /// What it may write: its disk, and the volumes it mounts to write.
    pub(super) writes: Vec<Written>,
    /// Whether its machine runs still.
    pub(super) up: AtomicBool,
    /// Each command sent to the machine and not yet answered, in the order sent.
    starting: Mutex<VecDeque<Starting>>,
    /// Where what each process on a terminal writes goes, by its pid, until it ends.
    terminals: Mutex<HashMap<u32, mpsc::UnboundedSender<String>>>,
    /// Who waits for each port asked of the machine and not yet answered, in the order asked.
    publishing: Mutex<VecDeque<Publishing>>,
    /// Who waits for the guest to say it has synced, and for its machine to say its disk is
    /// marked, and rebased, and at which mark.
    pub(super) synced: Mutex<Option<oneshot::Sender<()>>>,
    pub(super) marked: Mutex<Option<Waiting>>,
    pub(super) rebased: Mutex<Option<Waiting>>,
    /// Its jobs started with a stdin still open, by id.
    stdins: Mutex<HashSet<String>>,
}

impl Run {
    /// What it held let go of: its machine has ended.
    fn release(&self) {
        drop(self.held.lock().unwrap().take());
    }
}

/// A sandbox: a machine booted from an image over a disk of its own, with volumes mounted
/// into it, and the processes run in it as [jobs](Job), each with its log. It lasts from its
/// making until it is deleted, running or stopped; what it wrote is on its disk across its
/// stops, and what ran in it is not.
pub struct Sandbox {
    pub(super) record: Record,
    pub(super) shared: Arc<Shared>,
    state: Mutex<State>,
    /// Held while it starts, stops or is deleted, so one does at a time.
    pub(super) turn: tokio::sync::Mutex<()>,
    /// Its jobs, by id, until each is removed or it is deleted: a stop ends them, and keeps
    /// them.
    jobs: Mutex<BTreeMap<String, Arc<Job>>>,
    /// Where its machine's ports are reached, whichever machine runs.
    ports: Ports,
    /// What answers each port, by port, where [`Hooks::listen`] gave something to.
    listening: Mutex<HashMap<u16, Held>>,
    /// How many [`Activity`]s are under way.
    active: AtomicUsize,
    /// Whether its terminals were asked if a command runs in them, and none has answered that
    /// one does, nor been typed in, since.
    pub(super) asked: AtomicBool,
}

impl Sandbox {
    pub(super) fn new(record: Record, shared: Arc<Shared>) -> Arc<Sandbox> {
        Arc::new(Sandbox {
            record,
            shared,
            state: Mutex::new(State::Stopped),
            turn: tokio::sync::Mutex::new(()),
            jobs: Mutex::default(),
            ports: Ports::default(),
            listening: Mutex::default(),
            active: AtomicUsize::new(0),
            asked: AtomicBool::new(false),
        })
    }

    pub fn id(&self) -> &str {
        &self.record.id
    }

    pub fn name(&self) -> Option<&str> {
        self.record.name.as_deref()
    }

    /// What it was made with: its `image` the digest it stands on.
    pub fn args(&self) -> &SandboxCreateArgs {
        &self.record.args
    }

    pub fn ephemeral(&self) -> bool {
        self.record.ephemeral
    }

    /// Whether its machine runs.
    pub fn is_running(&self) -> bool {
        self.run().is_some()
    }

    /// It, as the protocol answers a sandbox, with its ports or not.
    pub fn answer(&self, ports: bool) -> virtx_core::Sandbox {
        self.record.answer(self.is_running(), ports)
    }

    /// Its run, while its machine runs.
    pub(super) fn run(&self) -> Option<Arc<Run>> {
        match &*self.state.lock().unwrap() {
            State::Running(run) if run.up.load(Ordering::Relaxed) => Some(Arc::clone(run)),
            _ => None,
        }
    }

    fn is_deleted(&self) -> bool {
        matches!(*self.state.lock().unwrap(), State::Deleted)
    }

    fn gone(&self) -> Error {
        Error::NotFound(format!("no sandbox here has the id {}", self.record.id))
    }

    /// Used now: what auto-stop counts from.
    pub(super) fn used(&self) {
        *self.record.used.lock().unwrap() = SystemTime::now();
        self.asked.store(false, Ordering::Relaxed);
    }

    /// When it was last used.
    pub(super) fn used_at(&self) -> SystemTime {
        *self.record.used.lock().unwrap()
    }

    /// Its record written where it is kept, as it is now, unless it is deleted.
    pub(super) fn keep(&self) {
        if self.is_deleted() {
            return;
        }
        let path = self.shared.kept.join(format!("{}.json", self.record.id));
        let written = serde_json::to_vec_pretty(&self.record.kept())
            .map_err(io::Error::other)
            .and_then(|json| std::fs::write(&path, json));
        if let Err(e) = written {
            eprintln!(
                "sandbox {} not kept at {}: {e}",
                self.record.id,
                path.display()
            );
        }
    }

    /// Started if it is stopped, over its disk as it was left: answered with its ports.
    pub async fn start(self: &Arc<Self>) -> Result<virtx_core::Sandbox, Error> {
        self.running().await?;
        Ok(self.record.answer(true, true))
    }

    /// Its run: its machine booted first if it is stopped.
    pub(super) async fn running(self: &Arc<Self>) -> Result<Arc<Run>, Error> {
        if let Some(run) = self.run() {
            return Ok(run);
        }
        let run = {
            let _turn = self.turn.lock().await;
            self.boot().await?
        };
        self.listen().await;
        Ok(run)
    }

    /// Its machine booted, over its disk as it was left -- held here, and by the hooks as they
    /// hold it -- with its volumes mounted and its ports published; or the one that runs
    /// already. Called under its turn.
    async fn boot(self: &Arc<Self>) -> Result<Arc<Run>, Error> {
        match &*self.state.lock().unwrap() {
            State::Running(run) if run.up.load(Ordering::Relaxed) => return Ok(Arc::clone(run)),
            State::Deleted => return Err(self.gone()),
            _ => {}
        }
        let shared = &self.shared;
        let id = self.record.id.clone();
        let spec = spec_of(&self.record.args_now())?;
        let (image, layers) = shared
            .images
            .get_image(&self.record.image)?
            .ok_or_else(|| Error::NotFound(format!("no image here is {}", self.record.image)))?;
        let began = Instant::now();
        let mut held = shared.hooks.booting(self, &spec).await?;

        let disk = shared.snapshots.open(&id)?;
        // Held a moment -- copied to be kept, as it stopped -- waited for; held longer, as by
        // a machine another server runs it in, refused.
        let mut waited = Duration::ZERO;
        while disk.in_use()? {
            if waited >= DISK_WAIT {
                return Err(SnapshotError::InUse(id).into());
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
            waited += Duration::from_millis(200);
        }
        // Grown since it last booted: its filesystem written again as large.
        let wanted = self.record.disk_size();
        if disk.capacity()? < wanted {
            shared.hooks.materialize(&id, &disk.path).await?;
            let growing = disk.clone();
            tokio::task::spawn_blocking(move || growing.grow(wanted)).await??;
        }
        // Held from here until its machine has ended, against every other machine, removal
        // and copy of it, in this process or another.
        let lock = disk.lock()?;
        let volumes = self
            .record
            .args
            .volumes
            .iter()
            .map(|asked| {
                Ok(VolumeMount {
                    path: shared.volumes.open(&asked.volume)?.path().to_owned(),
                    at: asked.path.clone(),
                    read_only: asked.readonly,
                })
            })
            .collect::<Result<_, Error>>()?;
        let writes: Vec<_> = std::iter::once(Written::Disk(id.clone()))
            .chain(
                self.record
                    .args
                    .volumes
                    .iter()
                    .filter(|asked| !asked.readonly)
                    .map(|asked| Written::Volume(asked.volume.clone())),
            )
            .collect();
        shared.hooks.changed(&writes)?;

        // Merging the layers' metadata is blocking work.
        let images = shared.images.clone();
        let rootfs = tokio::task::spawn_blocking(move || images.insert_rootfs(&layers)).await??;
        let ports = (self.record.opened.lock().unwrap().iter())
            .map(|open| open.port)
            .collect();
        let chunks = match shared.hooks.chunks(&id, &disk.path)? {
            Some((socket, serving)) => {
                held.push(serving);
                Some(socket)
            }
            None => None,
        };
        let lazily = chunks.is_some();
        // The programs it was made with, as `/abin`: the same directory for every sandbox here
        // made with the same, which none writes to.
        let abin = match self.record.args.abins.is_empty() {
            true => None,
            false => {
                let programs = abin_programs(&shared.vm);
                let under = shared.kept.join(ABIN_HOST);
                let names = self.record.args.abins.clone();
                let made = move || abin_of(&under, &programs, &names);
                Some(tokio::task::spawn_blocking(made).await??)
            }
        };
        let config = MachineConfig {
            rootfs,
            config: image.config().clone().unwrap_or_default(),
            snapshot: Some(disk),
            snapshot_held: true,
            spec,
            volumes,
            abin,
            ports,
            chunks,
        };

        // Booting is blocking work, until the guest takes commands.
        let vm = shared.vm.clone();
        let machine = tokio::task::spawn_blocking(move || Machine::start(config, &vm)).await??;
        shared.hooks.booted(&id, began.elapsed(), lazily);
        let run = Arc::new(Run {
            machine: Arc::new(machine),
            held: Mutex::new(Some((lock, held))),
            writes,
            up: AtomicBool::new(true),
            starting: Mutex::default(),
            terminals: Mutex::default(),
            publishing: Mutex::default(),
            synced: Mutex::default(),
            marked: Mutex::default(),
            rebased: Mutex::default(),
            stdins: Mutex::default(),
        });
        *self.state.lock().unwrap() = State::Running(Arc::clone(&run));
        self.used();
        let (reading, read) = (Arc::clone(self), Arc::clone(&run));
        std::thread::spawn(move || reading.read(&read));
        self.keep();
        Ok(run)
    }

    /// Each of its ports that nothing answers yet given to [`Hooks::listen`].
    async fn listen(self: &Arc<Self>) {
        let opened = self.record.opened.lock().unwrap().clone();
        let mut changed = false;
        for port in opened {
            if self.listening.lock().unwrap().contains_key(&port.port) {
                continue;
            }
            match self.shared.hooks.listen(self, &port).await {
                Ok(Some((url, held))) => {
                    self.listening.lock().unwrap().insert(port.port, held);
                    let mut opened = self.record.opened.lock().unwrap();
                    if let Some(open) = opened.iter_mut().find(|open| open.port == port.port) {
                        open.url = url;
                        changed = true;
                    }
                }
                Ok(None) => {}
                Err(e) => eprintln!(
                    "port {} of sandbox {} not answered: {e}",
                    port.port, self.record.id
                ),
            }
        }
        if changed {
            self.keep();
        }
    }

    /// Stopped, if it runs: its jobs ended, its machine stopped with what the guest wrote on
    /// its disk, and its disk kept.
    pub(super) async fn stop(self: &Arc<Self>) -> Result<(), Error> {
        let _turn = self.turn.lock().await;
        self.stop_turned().await
    }

    /// As [`stop`](Self::stop), under its turn.
    async fn stop_turned(self: &Arc<Self>) -> Result<(), Error> {
        let Some(run) = self.take_run()? else {
            return Ok(());
        };
        let stopping = Arc::clone(self);
        tokio::task::spawn_blocking(move || stopping.halt(&run)).await?;
        self.keep();
        Ok(())
    }

    /// Its run, taken out to be stopped: what it may write marked changed first, so that it
    /// is kept once its machine has ended, not let go of meanwhile.
    fn take_run(&self) -> Result<Option<Arc<Run>>, Error> {
        let run = {
            let mut state = self.state.lock().unwrap();
            match &*state {
                State::Deleted => return Err(self.gone()),
                State::Stopped => return Ok(None),
                State::Running(run) => {
                    let run = Arc::clone(run);
                    *state = State::Stopped;
                    run
                }
            }
        };
        if let Err(e) = self.shared.hooks.changed(&run.writes) {
            eprintln!("sandbox {}, stopped, not to be kept: {e}", self.record.id);
        }
        Ok(Some(run))
    }

    /// `run`'s machine stopped, and every job in it ended as cancelling it does: the jobs stay.
    /// Blocks while the guest puts what it wrote on its disk.
    fn halt(&self, run: &Run) {
        run.machine.stop();
        for job in self.jobs.lock().unwrap().values() {
            job.end(128 + SIGKILL);
        }
        run.release();
    }

    /// As [`stop`](Self::stop), blocking: not on an async runtime's thread.
    pub(super) fn stop_blocking(&self) {
        let _turn = self.turn.blocking_lock();
        if let Ok(Some(run)) = self.take_run() {
            self.halt(&run);
            self.keep();
        }
    }

    /// Deleted: stopped first if it runs, its disk removed -- from wherever else it is kept
    /// first -- and its jobs and its record with it. Its volumes stay.
    pub(super) async fn delete(self: &Arc<Self>) -> Result<(), Error> {
        let _turn = self.turn.lock().await;
        self.stop_turned().await?;
        let id = &self.record.id;
        self.shared.hooks.deleting(id).await?;
        match self.shared.snapshots.remove(id) {
            Ok(()) | Err(SnapshotError::Unknown(_)) => {}
            Err(e) => return Err(e.into()),
        }
        self.forgotten();
        Ok(())
    }

    /// As [`delete`](Self::delete), blocking and with nothing else told: what a server going
    /// does with an ephemeral sandbox.
    pub(super) fn delete_blocking(&self) {
        self.stop_blocking();
        let _turn = self.turn.blocking_lock();
        if self.shared.snapshots.remove(&self.record.id).is_ok() {
            self.forgotten();
        }
    }

    /// Let go of here, its disk left as it is: its record and its jobs gone, as a delete does,
    /// nothing removed from where its disk is kept. Refused while it runs.
    pub(super) async fn forget(&self) -> Result<(), Error> {
        let _turn = self.turn.lock().await;
        match &*self.state.lock().unwrap() {
            State::Deleted => return Err(self.gone()),
            State::Running(_) => {
                return Err(Error::Running(format!(
                    "sandbox {} runs here: stop it first",
                    self.record.id
                )));
            }
            State::Stopped => {}
        }
        self.forgotten();
        Ok(())
    }

    /// Gone from here: its record, its jobs and its ports.
    fn forgotten(&self) {
        *self.state.lock().unwrap() = State::Deleted;
        for job in std::mem::take(&mut *self.jobs.lock().unwrap()).into_values() {
            job.remove();
        }
        self.listening.lock().unwrap().clear();
        let _ = std::fs::remove_file(self.shared.kept.join(format!("{}.json", self.record.id)));
    }

    /// Start `cmd` in it, beside whatever else runs there -- started first if it is stopped --
    /// with a stdin to write to if `stdin`, and kill it after `timeout`.
    pub async fn exec(
        self: &Arc<Self>,
        cmd: Vec<String>,
        stdin: bool,
        timeout: Option<Duration>,
    ) -> Result<Arc<Job>, Error> {
        if cmd.is_empty() {
            return Err(Error::Invalid("cmd: an exec names a command".into()));
        }
        // In use until the job runs, which keeps it in use from there.
        let _activity = self.activity();
        let run = self.running().await?;
        let (waiter, started) = oneshot::channel();
        {
            let mut starting = run.starting.lock().unwrap();
            if !run.up.load(Ordering::Relaxed) {
                return Err(Error::Ended);
            }
            run.machine.exec(cmd.clone(), stdin)?;
            starting.push_back(Starting {
                cmd,
                terminal: None,
                waiter,
            });
        }
        let job = started
            .await
            .map_err(|_| Error::Ended)?
            .map_err(Error::NotStarted)?;
        if stdin {
            run.stdins.lock().unwrap().insert(job.id());
        }
        if let Some(timeout) = timeout {
            let (run, job) = (Arc::downgrade(&run), Arc::clone(&job));
            tokio::spawn(async move {
                tokio::time::sleep(timeout).await;
                if let Some(run) = run.upgrade() {
                    kill(&run, &job);
                }
            });
        }
        Ok(job)
    }

    /// `cmd` on a terminal of its own, `cols` by `rows`: a job of it, as an exec is, which ends
    /// with the [`Terminal`]. Started first if it is stopped.
    pub async fn terminal(
        self: &Arc<Self>,
        cmd: Vec<String>,
        cols: u16,
        rows: u16,
    ) -> Result<Terminal, Error> {
        let _activity = self.activity();
        let run = self.running().await?;
        let (waiter, started) = oneshot::channel();
        let (output, read) = mpsc::unbounded_channel();
        {
            let mut starting = run.starting.lock().unwrap();
            if !run.up.load(Ordering::Relaxed) {
                return Err(Error::Ended);
            }
            run.machine.exec_tty(cmd.clone(), cols, rows)?;
            starting.push_back(Starting {
                cmd,
                terminal: Some(output),
                waiter,
            });
        }
        let job = started
            .await
            .map_err(|_| Error::Ended)?
            .map_err(Error::NotStarted)?;
        let pid = job.pid().0;
        Ok(Terminal {
            sandbox: Arc::clone(self),
            run,
            job,
            pid,
            output: read,
        })
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
        if let Some(run) = self.run() {
            kill(&run, &job);
        }
        job.remove();
        Ok(())
    }

    /// Send the job `id` `signal`, as [`signal_number`] numbers it.
    pub fn signal(&self, id: &str, signal: i32) -> Result<(), Error> {
        let (pid, running) = self.job(id)?.pid();
        let run = self.run().filter(|_| running);
        let run = run.ok_or(Error::JobEnded("the job has ended"))?;
        run.machine.signal(pid, signal)?;
        Ok(())
    }

    /// Write `data` to the stdin of the job `id`, started with one, and close it if `eof`.
    pub fn write_stdin(&self, id: &str, data: String, eof: bool) -> Result<(), Error> {
        let (pid, running) = self.job(id)?.pid();
        let ended = Error::JobEnded(
            "the job has ended, or its stdin is closed, or it was started without one",
        );
        let Some(run) = self.run().filter(|_| running) else {
            return Err(ended);
        };
        let mut stdins = run.stdins.lock().unwrap();
        if !stdins.contains(id) {
            return Err(ended);
        }
        run.machine.write_stdin(pid, data, eof)?;
        if eof {
            stdins.remove(id);
        }
        Ok(())
    }

    /// Port `port` opened, as `label` and `protocol` say -- TCP where it says none -- as those
    /// it was made with are, and published at once in its machine if it runs: answered with
    /// where it is reached, and whether it was opened now rather than open already. One open
    /// already takes the label asked for, and refuses another protocol.
    pub async fn open_port(
        self: &Arc<Self>,
        port: u16,
        label: Option<String>,
        protocol: Option<PortProtocol>,
    ) -> Result<(SandboxPort, bool), Error> {
        if port == 0 {
            return Err(Error::Invalid("port: 0 is not one".into()));
        }
        if !label_fits(label.as_deref()) {
            return Err(Error::Invalid(format!(
                "label: past {LABEL_MOST} characters"
            )));
        }
        if self.is_deleted() {
            return Err(self.gone());
        }
        let already = |opened: &mut Vec<SandboxPort>| -> Option<Result<SandboxPort, Error>> {
            let open = opened.iter_mut().find(|open| open.port == port)?;
            if protocol.is_some_and(|protocol| protocol != open.protocol) {
                return Some(Err(Error::Exists(
                    format!("port {port} is open as {:?} already", open.protocol).to_lowercase(),
                )));
            }
            if label.is_some() && label != open.label {
                open.label = label.clone();
            }
            Some(Ok(open.clone()))
        };
        let was = already(&mut self.record.opened.lock().unwrap());
        if let Some(was) = was {
            let open = was?;
            self.keep();
            return Ok((open, false));
        }
        // A running one's machine publishes it now; a stopped one's, as it boots.
        if let Some(run) = self.run() {
            let (told, answer) = oneshot::channel();
            {
                // Asked and waited for in one turn, so each answer reaches the one who asked.
                let mut publishing = run.publishing.lock().unwrap();
                run.machine.publish(port)?;
                publishing.push_back(told);
            }
            match tokio::time::timeout(PUBLISH_WAIT, answer).await {
                Ok(Ok(Ok(_))) => {}
                Ok(Ok(Err(why))) => return Err(Error::Invalid(why)),
                Ok(Err(_)) => return Err(Error::Ended),
                Err(_) => {
                    return Err(Error::Failed(format!(
                        "its machine did not publish port {port}"
                    )));
                }
            }
        }
        let made = {
            let mut opened = self.record.opened.lock().unwrap();
            // Opened meanwhile by another request: the same port, the same URL.
            if let Some(was) = already(&mut opened) {
                return Ok((was?, false));
            }
            let spec = PortSpec {
                port,
                label,
                protocol: protocol.unwrap_or_default(),
                inbound: None,
            };
            let (key, url) = self.shared.hooks.port(&spec, None);
            let made = SandboxPort {
                port,
                url,
                label: spec.label,
                protocol: spec.protocol,
                // A public address is the cloud's to give (its gateway), not a server's.
                address: None,
            };
            opened.push(made.clone());
            if let Some(key) = key {
                self.record.keys.lock().unwrap().insert(key, port);
            }
            made
        };
        self.listen().await;
        let made = self
            .record
            .opened
            .lock()
            .unwrap()
            .iter()
            .find(|open| open.port == port)
            .cloned()
            .unwrap_or(made);
        self.keep();
        Ok((made, true))
    }

    /// Port `port` closed: its URL reaches it no more from now on, and it is not opened again
    /// as it starts. A connection through it open now runs on until it ends.
    pub fn close_port(&self, port: u16) -> Result<(), Error> {
        if self.is_deleted() {
            return Err(self.gone());
        }
        {
            let mut opened = self.record.opened.lock().unwrap();
            let before = opened.len();
            opened.retain(|open| open.port != port);
            if opened.len() == before {
                return Err(Error::NotFound(format!(
                    "port {port} is not open in sandbox {}",
                    self.record.id
                )));
            }
            self.record
                .keys
                .lock()
                .unwrap()
                .retain(|_, opened| *opened != port);
        }
        self.listening.lock().unwrap().remove(&port);
        self.keep();
        Ok(())
    }

    /// The port key `key` names, if it is one of its own.
    pub(super) fn keyed(&self, key: &str) -> Option<u16> {
        self.record.keys.lock().unwrap().get(key).copied()
    }

    /// Its machine while it runs, to ask of its files: refused while it is stopped, which a
    /// request to its files does not start. The caller holds an [`Activity`] for as long as it
    /// asks.
    pub fn running_machine(&self) -> Result<Arc<Machine>, Error> {
        if self.is_deleted() {
            return Err(self.gone());
        }
        match self.run() {
            Some(run) => Ok(Arc::clone(&run.machine)),
            None => Err(Error::Stopped(format!(
                "sandbox {} is stopped: its files are read and written while it runs",
                self.record.id
            ))),
        }
    }

    /// A connection to `port` in its machine. Refused while it is stopped, as a port nothing
    /// listens on is: it is not started for it.
    pub async fn connect(&self, port: u16) -> io::Result<TcpStream> {
        self.ports.connect(port).await
    }

    /// Something done with it from outside, such as a connection through a port or a request
    /// to its files: it is in use until it drops.
    pub fn activity(self: &Arc<Self>) -> Activity {
        self.active.fetch_add(1, Ordering::Relaxed);
        Activity(Arc::clone(self))
    }

    /// Whether it is in use now: an [`Activity`] under way, or a job running that is not on a
    /// terminal -- which counts as use only while it is typed in, or runs a command.
    pub(super) fn busy(&self) -> bool {
        if self.active.load(Ordering::Relaxed) > 0 {
            return true;
        }
        let terminals: HashSet<u32> = match self.run() {
            Some(run) => run.terminals.lock().unwrap().keys().copied().collect(),
            None => HashSet::new(),
        };
        let jobs = self.jobs.lock().unwrap();
        jobs.values().any(|job| {
            let (pid, running) = job.pid();
            running && !terminals.contains(&pid)
        })
    }

    /// The processes on its terminals, by pid.
    pub(super) fn terminals(&self) -> Vec<u32> {
        match self.run() {
            Some(run) => run.terminals.lock().unwrap().keys().copied().collect(),
            None => Vec::new(),
        }
    }

    /// Asked whether the process `pid`, on a terminal, runs a command: a busy one is used.
    pub(super) fn ask_foreground(&self, pid: u32) {
        if let Some(run) = self.run() {
            let _ = run.machine.foreground(pid);
        }
    }

    /// How many of its jobs run now.
    pub(super) fn running_jobs(&self) -> usize {
        let jobs = self.jobs.lock().unwrap();
        jobs.values().filter(|job| job.pid().1).count()
    }

    /// What its machine may write while it runs.
    pub(super) fn writes(&self) -> Vec<Written> {
        self.run().map(|run| run.writes.clone()).unwrap_or_default()
    }

    /// Its disk to grow to `gib` GiB as it next starts -- never shrunk: smaller than it is,
    /// refused. Whether it runs, to be grown once it starts again.
    pub fn grow_disk(&self, gib: u32) -> Result<bool, Error> {
        let wanted = u64::from(gib) << 30;
        let now = self.disk_now();
        if wanted < now {
            return Err(Error::Invalid(format!(
                "disk_gib: a disk is grown, never shrunk -- it is {} GiB",
                now.div_ceil(1 << 30)
            )));
        }
        *self.record.disk_gib.lock().unwrap() = Some(gib);
        self.keep();
        Ok(self.is_running())
    }

    /// The bytes its disk is, or is to be as it next starts.
    fn disk_now(&self) -> u64 {
        let length = self
            .shared
            .snapshots
            .open(&self.record.id)
            .ok()
            .and_then(|disk| disk.capacity().ok())
            .unwrap_or(0);
        self.record.disk_size().max(length)
    }

    /// Its machine resized, and its disk grown, as it next starts: its vCPUs and memory only
    /// while it is stopped, a machine being the size it booted as; its disk never shrunk.
    /// Answered as it is to be.
    pub async fn resize(
        &self,
        vcpus: Option<u8>,
        memory_mib: Option<u32>,
        disk_gib: Option<u32>,
    ) -> Result<Resources, Error> {
        // Not started meanwhile, nor stopped.
        let _turn = self.turn.lock().await;
        if self.is_deleted() {
            return Err(self.gone());
        }
        let now = self.record.args_now();
        let changes = |asked: Option<u32>, is: Option<u32>| asked.is_some_and(|a| Some(a) != is);
        if (changes(vcpus.map(u32::from), now.vcpus.map(u32::from))
            || changes(memory_mib, now.memory_mib))
            && self.is_running()
        {
            return Err(Error::Running(
                "its vCPUs and memory change while it is stopped: stop it first".into(),
            ));
        }
        let resized = Resized {
            vcpus: vcpus.or(now.vcpus),
            memory_mib: memory_mib.or(now.memory_mib),
        };
        spec_of(&SandboxCreateArgs {
            vcpus: resized.vcpus,
            memory_mib: resized.memory_mib,
            ..now.clone()
        })?;
        if let Some(gib) = disk_gib {
            let is = self.disk_now();
            if (u64::from(gib) << 30) < is {
                return Err(Error::Invalid(format!(
                    "disk_gib: a disk is grown, never shrunk -- it is {} GiB",
                    is.div_ceil(1 << 30)
                )));
            }
            *self.record.disk_gib.lock().unwrap() = Some(gib);
        }
        *self.record.resized.lock().unwrap() = resized;
        self.keep();
        let now = self.record.args_now();
        Ok(Resources {
            vcpus: now.vcpus.unwrap_or(DEFAULT_VCPUS),
            memory_mib: now.memory_mib.unwrap_or(DEFAULT_MEMORY_MIB),
            disk_gib: self.record.disk_size().div_ceil(1 << 30) as u32,
            running: self.is_running(),
        })
    }

    /// What `run`'s machine says, until it ends: each command's pid, as the job its waiter
    /// gets, each job's output and end, and the answers to what was asked of it.
    fn read(&self, run: &Arc<Run>) {
        let machine = &run.machine;
        let _published = self.ports.publish(machine.published.clone());
        // Those opened since, each published until the machine ends, as those it booted with are.
        let mut since = Vec::new();
        let mut running = HashMap::new();
        while let Some(message) = machine.recv() {
            match message {
                CtrlResp::Started(started) => {
                    let Some(Starting {
                        cmd,
                        terminal,
                        waiter,
                    }) = run.starting.lock().unwrap().pop_front()
                    else {
                        continue;
                    };
                    if let (Ok(pid), Some(terminal)) = (&started, terminal) {
                        run.terminals.lock().unwrap().insert(*pid, terminal);
                    }
                    let started = started.map(|pid| {
                        self.used();
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
                CtrlResp::Output { id, data } => {
                    if let Some(terminal) = run.terminals.lock().unwrap().get(&id) {
                        let _ = terminal.send(data);
                    }
                }
                CtrlResp::Exited { id, code } => {
                    run.terminals.lock().unwrap().remove(&id);
                    if let Some(job) = running.remove(&id) {
                        self.used();
                        job.end(code);
                    }
                }
                CtrlResp::Log { level, message } => {
                    self.shared.hooks.said(&self.record.id, &level, &message)
                }
                CtrlResp::Synced => {
                    if let Some(told) = run.synced.lock().unwrap().take() {
                        let _ = told.send(());
                    }
                }
                CtrlResp::Marked { mark, marked } => {
                    let mut waiting = run.marked.lock().unwrap();
                    if waiting.as_ref().is_some_and(|(m, _)| *m == mark)
                        && let Some((_, told)) = waiting.take()
                    {
                        let _ = told.send(marked);
                    }
                }
                CtrlResp::Rebased { mark, rebased } => {
                    let mut waiting = run.rebased.lock().unwrap();
                    if waiting.as_ref().is_some_and(|(m, _)| *m == mark)
                        && let Some((_, told)) = waiting.take()
                    {
                        let _ = told.send(rebased);
                    }
                }
                CtrlResp::Published(published) => {
                    if let Ok(at) = published {
                        since.push(self.ports.publish(vec![at]));
                    }
                    if let Some(told) = run.publishing.lock().unwrap().pop_front() {
                        let _ = told.send(published);
                    }
                }
                // A command runs on a terminal: used, as long as it does.
                CtrlResp::Foreground { busy, .. } => {
                    if busy {
                        self.used();
                    }
                }
                CtrlResp::Ready(_) => {}
            }
        }
        // Gone, and what ran in it with it: killed, as far as anything in it can tell.
        let mut starting = run.starting.lock().unwrap();
        run.up.store(false, Ordering::Relaxed);
        starting.clear();
        drop(starting);
        run.terminals.lock().unwrap().clear();
        run.publishing.lock().unwrap().clear();
        for job in running.into_values() {
            job.end(128 + SIGKILL);
        }
        // Ended on its own, not stopped: stopped now, and started again as anything asks.
        let ended = {
            let mut state = self.state.lock().unwrap();
            let ended = matches!(&*state, State::Running(up) if Arc::ptr_eq(up, run));
            if ended {
                *state = State::Stopped;
            }
            ended
        };
        if ended {
            if let Err(e) = self.shared.hooks.changed(&run.writes) {
                eprintln!("sandbox {}, ended, not to be kept: {e}", self.record.id);
            }
            eprintln!("sandbox {}'s machine ended on its own", self.record.id);
        }
        run.release();
    }
}

/// What a sandbox's machine is, as resized: what [`Sandbox::resize`] answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Resources {
    pub vcpus: u8,
    pub memory_mib: u32,
    pub disk_gib: u32,
    pub running: bool,
}

/// Kill `job` if it runs, in `run`'s machine.
fn kill(run: &Run, job: &Job) {
    let (pid, running) = job.pid();
    if running && run.up.load(Ordering::Relaxed) {
        let _ = run.machine.signal(pid, SIGKILL);
    }
}

/// What `args` boots: its machine's size and network, refused for what no machine is.
pub(super) fn spec_of(args: &SandboxCreateArgs) -> Result<MachineSpec, Error> {
    let mut at = std::collections::HashSet::new();
    for volume in &args.volumes {
        // Asked of the text rather than of `Path`, which answers by this host's rules.
        let path = volume.path.trim_end_matches('/');
        let why = if !volume.path.starts_with('/') {
            "a path in the guest is absolute"
        } else if path.is_empty() {
            "/ is the guest's root"
        } else if !at.insert(path) {
            "another volume is there"
        } else {
            continue;
        };
        return Err(Error::Invalid(format!(
            "volumes: {} at {}: {why}",
            volume.volume, volume.path
        )));
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
    Ok(MachineSpec {
        vcpus: args.vcpus.unwrap_or(DEFAULT_VCPUS),
        ram_mib: args.memory_mib.unwrap_or(DEFAULT_MEMORY_MIB),
        gpu,
        gpu_memory_mib: args.gpu_memory_mib,
        network: match args.network {
            true => Network::Public,
            false => Network::None,
        },
    })
}

pub(super) fn label_fits(label: Option<&str>) -> bool {
    label.is_none_or(|label| label.chars().count() <= LABEL_MOST)
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
        self.0.used();
        self.0.active.fetch_sub(1, Ordering::Relaxed);
    }
}

/// A process on a terminal in a sandbox, as [`Sandbox::terminal`] starts it: its job, what it
/// writes, and how to type into it, resize it and end it.
pub struct Terminal {
    sandbox: Arc<Sandbox>,
    run: Arc<Run>,
    job: Arc<Job>,
    pid: u32,
    /// What it writes, as it comes; ended once it has.
    pub output: mpsc::UnboundedReceiver<String>,
}

impl Terminal {
    /// `data` typed into it: the sandbox used now.
    pub fn write(&self, data: String) -> Result<(), Error> {
        self.sandbox.used();
        Ok(self.run.machine.stdin(self.pid, data)?)
    }

    pub fn resize(&self, cols: u16, rows: u16) -> Result<(), Error> {
        Ok(self.run.machine.resize(self.pid, cols, rows)?)
    }

    /// How it ended, once it has: its exit code, `128 + signal` for one a signal ended.
    pub async fn ended(&self) -> i32 {
        self.job.ended().await
    }
}

impl Drop for Terminal {
    /// It, and what it started, killed if they run still: the terminal is gone.
    fn drop(&mut self) {
        kill(&self.run, &self.job);
    }
}

/// A name for what a client named nothing: eight hex digits for which `taken` is false.
pub(super) fn pick(taken: impl Fn(&str) -> bool) -> String {
    loop {
        let name = format!("{:08x}", RandomState::new().build_hasher().finish() as u32);
        if !taken(&name) {
            return name;
        }
    }
}

#[cfg(test)]
mod keeping_ended {
    use super::*;

    fn job(id: &str, ended: bool) -> (String, Arc<Job>) {
        let job = Job::new(SandboxJob {
            id: id.into(),
            kind: SandboxJobKind::Exec,
            pid: 1,
            cmd: vec!["true".into()],
            running: true,
            code: None,
        });
        if ended {
            job.end(0);
        }
        (id.into(), job)
    }

    #[test]
    fn the_ended_jobs_past_the_limit_go_the_first_ended_first_and_none_running() {
        let mut jobs = BTreeMap::new();
        for i in 0..5 {
            let (id, job) = job(&format!("e{i}"), true);
            jobs.insert(id, job);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let (id, running) = job("r", false);
        jobs.insert(id, running);
        // Room for one more under three ended: the three that ended first go.
        keep_ended(&mut jobs, 3);
        let mut left: Vec<&str> = jobs.keys().map(String::as_str).collect();
        left.sort();
        assert_eq!(left, ["e3", "e4", "r"]);
        // Under the limit: nothing goes.
        keep_ended(&mut jobs, 10);
        assert_eq!(jobs.len(), 3);
    }
}

/// Where a server's own programs for `/abin` are: `abin/` beside the program its machines run
/// as, as `cargo xtask install` and a release put them there.
pub(super) fn abin_programs(vm: &[OsString]) -> PathBuf {
    let dir = vm.first().and_then(|program| Path::new(program).parent());
    dir.map(|dir| dir.join(ABIN_HOST)).unwrap_or_default()
}

/// `abins` refused, as a sandbox is made with them, unless each is a program in `programs`,
/// given once.
pub(super) fn abins_refused(abins: &[String], programs: &Path) -> Result<(), Error> {
    // Every name given twice first, so what is said of a request does not hang on what this
    // server has.
    let mut given = HashSet::new();
    if let Some(name) = abins.iter().find(|name| !given.insert(*name)) {
        return Err(Error::Invalid(format!("abins: `{name}` is given twice")));
    }
    for name in abins {
        if name.starts_with('.') || name.contains(['/', '\\']) || !programs.join(name).is_file() {
            return Err(Error::Invalid(format!(
                "abins: this server has no program called `{name}`"
            )));
        }
    }
    Ok(())
}

/// The directory a machine of a sandbox made with `names` shows at `/abin`: each program in
/// `programs` it names, and its `SKILL.md` under `skills/` where it has one, linked -- or copied,
/// where a link cannot be, as across filesystems -- into a directory under `under`.
///
/// One for each set of names and the programs as they are, made the first time one is asked
/// for and kept: every machine shows it read-only, so the sandboxes made with the same share
/// it, and a boot after the first links and copies nothing. Programs replaced since are another
/// set, of their sizes and times.
fn abin_of(under: &Path, programs: &Path, names: &[String]) -> Result<PathBuf, Error> {
    let io = |context: String| move |source| SnapshotError::Io { context, source };
    let mut sorted: Vec<&String> = names.iter().collect();
    sorted.sort();
    let mut hasher = std::hash::DefaultHasher::new();
    for name in &sorted {
        let path = programs.join(name);
        let meta = fs_err(std::fs::metadata(&path), &path)?;
        hasher.write(name.as_bytes());
        hasher.write_u64(meta.len());
        if let Ok(modified) = meta.modified() {
            let since = modified
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default();
            hasher.write_u128(since.as_nanos());
        }
    }
    let set = sorted
        .iter()
        .map(|name| name.as_str())
        .collect::<Vec<_>>()
        .join("+");
    let dir = under.join(format!("{set}.{:016x}", hasher.finish()));
    if dir.is_dir() {
        return Ok(dir);
    }
    std::fs::create_dir_all(under).map_err(io(format!("making {}", under.display())))?;
    // Made aside, then put in place whole, so a boot never shows one half made.
    let making = under.join(format!(".{set}.{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&making);
    let made = (|| {
        for program in &sorted {
            link(&programs.join(program), &making.join(program))?;
            let skill = Path::new(SKILLS).join(program).join(SKILL);
            if programs.join(&skill).is_file() {
                link(&programs.join(&skill), &making.join(&skill))?;
            }
        }
        Ok::<_, Error>(())
    })();
    if let Err(e) = made {
        let _ = std::fs::remove_dir_all(&making);
        return Err(e);
    }
    if std::fs::rename(&making, &dir).is_err() {
        // Another boot put the same in place first.
        let _ = std::fs::remove_dir_all(&making);
        if !dir.is_dir() {
            return Err(SnapshotError::Io {
                context: format!("putting {} in place", dir.display()),
                source: io::Error::other("it is not there"),
            }
            .into());
        }
    }
    Ok(dir)
}

fn fs_err<T>(result: io::Result<T>, path: &Path) -> Result<T, Error> {
    result.map_err(|source| {
        SnapshotError::Io {
            context: format!("reading {}", path.display()),
            source,
        }
        .into()
    })
}

/// `from` at `to`, making its directory: a hard link, or a copy where `to` cannot be one.
fn link(from: &Path, to: &Path) -> Result<(), Error> {
    let io = |context: String| move |source| SnapshotError::Io { context, source };
    let parent = to.parent().unwrap_or(to);
    std::fs::create_dir_all(parent).map_err(io(format!("making {}", parent.display())))?;
    if std::fs::hard_link(from, to).is_err() {
        std::fs::copy(from, to).map_err(io(format!("copying {}", from.display())))?;
    }
    Ok(())
}

#[cfg(test)]
mod abin_tests {
    use super::*;

    #[test]
    fn a_set_of_programs_is_made_once_and_shared_by_those_named_alike() {
        let host = tempfile::tempdir().unwrap();
        let programs = host.path().join(ABIN_HOST);
        std::fs::create_dir_all(programs.join(SKILLS).join("index")).unwrap();
        for program in ["index", "docread", "display"] {
            std::fs::write(programs.join(program), program).unwrap();
        }
        std::fs::write(programs.join(SKILLS).join("index").join(SKILL), "skill").unwrap();
        let under = host.path().join("kept").join(ABIN_HOST);

        let names = ["index".to_string(), "display".to_string()];
        let dir = abin_of(&under, &programs, &names).unwrap();
        assert_eq!(std::fs::read(dir.join("index")).unwrap(), b"index");
        assert_eq!(std::fs::read(dir.join("display")).unwrap(), b"display");
        assert!(!dir.join("docread").exists());
        assert_eq!(
            std::fs::read(dir.join(SKILLS).join("index").join(SKILL)).unwrap(),
            b"skill"
        );
        assert!(!dir.join(SKILLS).join("display").exists());

        let again = ["display".to_string(), "index".to_string()];
        assert_eq!(
            abin_of(&under, &programs, &again).unwrap(),
            dir,
            "in any order"
        );
        let other = abin_of(&under, &programs, &["docread".to_string()]).unwrap();
        assert_ne!(other, dir);
        assert_eq!(
            std::fs::read_dir(&under).unwrap().count(),
            2,
            "nothing half made left"
        );

        assert!(abins_refused(&names, &programs).is_ok());
        for refused in [
            vec!["index".to_string(), "index".to_string()],
            vec!["nope".to_string()],
            vec!["skills".to_string()],
            vec!["../abin".to_string()],
        ] {
            assert!(
                matches!(abins_refused(&refused, &programs), Err(Error::Invalid(_))),
                "{refused:?}"
            );
        }
    }
}
