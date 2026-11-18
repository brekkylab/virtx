use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::ffi::OsString;
use std::hash::{BuildHasher as _, Hasher as _, RandomState};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant, SystemTime};

use tokio::net::TcpStream;
use tokio::sync::oneshot;
use virtx_core::{SandboxCreateArgs, SandboxJob, SandboxJobEntry, SandboxJobKind};

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
/// The most `disk_gib` a sandbox may ask for. Formatting a disk keeps 64 KiB of its bitmaps in
/// memory for each GiB.
pub const MAX_DISK_GIB: u32 = 1024;
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

/// How long a sandbox booting waits for its disk to be let go of: by what keeps it, as it was
/// parked, or by a process spawned as it was held.
const DISK_WAIT: Duration = Duration::from_secs(10);

/// How long a sandbox is unused before its machine is parked, where `VIRTX_PARK_AFTER_MS` does
/// not say: not at all, its machine parked as nothing uses it any more.
const PARK_AFTER: Duration = Duration::ZERO;

/// How often a sandbox in use is looked at again, to park it once it is not: what bounds how
/// soon after it is it parked.
const LOOK_EVERY: Duration = Duration::from_millis(100);

/// How long a sandbox is unused before its machine is parked, `None` for never:
/// `VIRTX_PARK_AFTER_MS`, in milliseconds, `0` for never; or at once.
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
    /// How long each sandbox is unused before its machine is parked, `None` for never.
    pub(super) park_after: Option<Duration>,
}

/// Where a sandbox's machine is.
enum State {
    Up(Arc<Run>),
    /// Stopped while nothing used it, its disk kept: booted again for what next needs it.
    Parked,
    /// Ended on its own: nothing boots it again.
    Exited,
    Deleted,
}

/// A command sent to a sandbox's machine, not yet answered: what it is, and who waits for its
/// job.
struct Starting {
    cmd: Vec<String>,
    waiter: oneshot::Sender<Result<Arc<Job>, String>>,
}

/// Who waits for a machine's answer about its disk at a mark, and the mark.
type Waiting = (u64, oneshot::Sender<Result<bool, String>>);

/// A sandbox's run: the machine it booted, up until it stops, and what is asked of it.
pub(super) struct Run {
    pub(super) machine: Arc<Machine>,
    /// Its sandbox's id, and when its machine came up: what [`runs`](super::runs) is told as
    /// it comes up and as it goes down.
    sandbox: String,
    started: SystemTime,
    /// Its disk's lock, and what the hooks hold for it, until it has stopped.
    held: Mutex<Option<(SnapshotLock, Vec<Held>)>>,
    /// What it may write: its disk, and the volumes it mounts to write.
    pub(super) writes: Vec<Written>,
    /// Whether its machine runs still.
    pub(super) up: AtomicBool,
    /// Each command sent to the machine and not yet answered, in the order sent.
    starting: Mutex<VecDeque<Starting>>,
    /// Who waits for the guest to say it has synced, and for its machine to say its disk is
    /// marked, and rebased, and at which mark.
    pub(super) synced: Mutex<Option<oneshot::Sender<()>>>,
    pub(super) marked: Mutex<Option<Waiting>>,
    pub(super) rebased: Mutex<Option<Waiting>>,
    /// Its jobs started with a stdin still open, by id.
    stdins: Mutex<HashSet<String>>,
    /// Who waits for each write to a process's stdin to be [`Written`](CtrlResp::Written), by
    /// pid, in the order sent; and each process's turn to write, one write at a time
    /// ([`Run::write_stdin`]).
    written: Mutex<HashMap<u32, VecDeque<oneshot::Sender<()>>>>,
    turns: Mutex<HashMap<u32, Arc<tokio::sync::Mutex<()>>>>,
}

impl Run {
    /// `data` written to the stdin of process `pid`, closed after if `eof`, answered once the
    /// guest has written it (issue #159): one write at a time for each process, so a process
    /// that does not read holds back whoever writes to it, as PROTOCOL.md says of a job's stdin,
    /// and what it has not read never piles up in the guest.
    /// Refused as ended once the process, or the machine, is.
    async fn write_stdin(&self, pid: u32, data: String, eof: bool) -> Result<(), Error> {
        let turn = Arc::clone(self.turns.lock().unwrap().entry(pid).or_default());
        let _turn = turn.lock().await;
        let written = {
            // Sent and waited for in one order: the guest answers each process's in turn.
            let mut waiting = self.written.lock().unwrap();
            self.machine.write_stdin(pid, data, eof)?;
            let (told, written) = oneshot::channel();
            waiting.entry(pid).or_default().push_back(told);
            written
        };
        written
            .await
            .map_err(|_| Error::JobEnded("the job has ended"))
    }

    /// What it held let go of: its machine has ended, its run told as ended, once.
    fn release(&self) {
        if let Some(held) = self.held.lock().unwrap().take() {
            drop(held);
            super::runs::down(&self.sandbox, self.started);
        }
    }
}

/// A sandbox: a machine booted from an image over a disk of its own, with volumes mounted
/// into it, and the processes run in it as [jobs](Job), each with its log. It lasts from its
/// making until it is deleted. Its machine is parked once it has been unused for a while, and
/// booted again for the next job or request to its files: what it wrote is on its
/// disk across that, and what ran in it is not.
pub struct Sandbox {
    pub(super) record: Record,
    pub(super) shared: Arc<Shared>,
    state: Mutex<State>,
    /// Held while its machine boots or is parked, or it is resized or deleted, so one does at a
    /// time.
    pub(super) turn: tokio::sync::Mutex<()>,
    /// Its jobs, by id, until each is removed or it is deleted: a park ends them, and keeps
    /// them.
    jobs: Mutex<BTreeMap<String, Arc<Job>>>,
    /// Where its machine's ports are reached, whichever machine runs.
    ports: Ports,
    /// What answers each port, by port, where [`Hooks::listen`] gave something to.
    listening: Mutex<HashMap<u16, Held>>,
    /// How many [`Activity`]s are under way.
    active: AtomicUsize,
    /// What a conditional write to its files holds first as it lands (see
    /// [`Files`](crate::Files)).
    writing: Arc<tokio::sync::Mutex<()>>,
}

impl Sandbox {
    pub(super) fn new(record: Record, shared: Arc<Shared>) -> Arc<Sandbox> {
        let state = match *record.exited.lock().unwrap() {
            true => State::Exited,
            false => State::Parked,
        };
        Arc::new(Sandbox {
            record,
            shared,
            state: Mutex::new(state),
            turn: tokio::sync::Mutex::new(()),
            jobs: Mutex::default(),
            ports: Ports::default(),
            listening: Mutex::default(),
            active: AtomicUsize::new(0),
            writing: Arc::default(),
        })
    }

    /// What a conditional write to its files holds first as it lands (see
    /// [`Files`](crate::Files)).
    pub(crate) fn writing(&self) -> &Arc<tokio::sync::Mutex<()>> {
        &self.writing
    }

    /// Each volume it mounts writable: where in the guest, spelled as the guest is given it,
    /// and the volume's own directory.
    pub(crate) fn mounts(&self) -> Vec<(String, std::path::PathBuf)> {
        self.record
            .args
            .volumes
            .iter()
            .filter(|asked| !asked.readonly)
            .filter_map(|asked| {
                let dir = self
                    .shared
                    .volumes
                    .open(&asked.volume)
                    .ok()?
                    .path()
                    .to_owned();
                Some((crate::mount_point(&asked.path).ok()?, dir))
            })
            .collect()
    }

    pub fn id(&self) -> &str {
        &self.record.id
    }

    /// Whether a sandbox lives under the home `root`, its machine up or not, of this process or
    /// another: a record of one kept there, or a disk there a machine holds.
    pub fn lives_under(root: &std::path::Path) -> bool {
        let Ok(entries) = std::fs::read_dir(root.join("sandbox")) else {
            return false;
        };
        entries.flatten().any(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.ends_with(".json") {
                return true;
            }
            name.ends_with(".ext4.lock")
                && std::fs::File::open(entry.path()).is_ok_and(|file| {
                    matches!(file.try_lock(), Err(std::fs::TryLockError::WouldBlock))
                })
        })
    }

    pub fn name(&self) -> Option<&str> {
        self.record.name.as_deref()
    }

    /// What it was made with: its `image` the digest it stands on.
    pub fn args(&self) -> &SandboxCreateArgs {
        &self.record.args
    }

    /// Whether its machine is up now: not parked, nor exited.
    pub fn up(&self) -> bool {
        self.run().is_some()
    }

    /// When its machine came up, if it is up now.
    pub fn up_since(&self) -> Option<SystemTime> {
        self.run().map(|run| run.started)
    }

    /// It, as the protocol answers a sandbox, with its ports or not.
    pub fn answer(&self, ports: bool) -> virtx_core::Sandbox {
        self.record.answer(ports)
    }

    /// Its run, while its machine runs.
    pub(super) fn run(&self) -> Option<Arc<Run>> {
        match &*self.state.lock().unwrap() {
            State::Up(run) if run.up.load(Ordering::Relaxed) => Some(Arc::clone(run)),
            _ => None,
        }
    }

    /// Whether `run` is its machine's run now.
    fn is(&self, run: &Arc<Run>) -> bool {
        matches!(&*self.state.lock().unwrap(), State::Up(up) if Arc::ptr_eq(up, run))
    }

    fn is_deleted(&self) -> bool {
        matches!(*self.state.lock().unwrap(), State::Deleted)
    }

    fn gone(&self) -> Error {
        Error::NotFound(format!("no sandbox here has the id {}", self.record.id))
    }

    /// Used now: what parking it, and reclaiming it, count from.
    pub(super) fn used(&self) {
        *self.record.used.lock().unwrap() = SystemTime::now();
    }

    /// When it was made, or last used: a job started, run or ended in it, a connection through
    /// its ports, a request to its files.
    pub fn last_used(&self) -> SystemTime {
        *self.record.used.lock().unwrap()
    }

    /// How long it has been unused: nothing in use now ([`busy`](Self::busy)), and last used
    /// that long ago. `None` while it is in use.
    pub(super) fn idle(&self) -> Option<Duration> {
        if self.busy() {
            return None;
        }
        Some(
            SystemTime::now()
                .duration_since(self.last_used())
                .unwrap_or_default(),
        )
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

    /// Its machine booted if it is parked, over its disk as it was left: answered with its
    /// ports. Refused once it has exited.
    pub async fn start(self: &Arc<Self>) -> Result<virtx_core::Sandbox, Error> {
        self.wake().await?;
        Ok(self.record.answer(true))
    }

    /// Its run: its machine booted first if it is parked.
    pub(super) async fn wake(self: &Arc<Self>) -> Result<Arc<Run>, Error> {
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
    /// hold it -- with its volumes mounted and its ports published, to be parked once it has
    /// been unused for its server's `park_after`; or the one that runs already. Refused once it
    /// has exited. Called under its turn.
    async fn boot(self: &Arc<Self>) -> Result<Arc<Run>, Error> {
        match &*self.state.lock().unwrap() {
            State::Up(run) if run.up.load(Ordering::Relaxed) => return Ok(Arc::clone(run)),
            State::Deleted => return Err(self.gone()),
            State::Exited => return Err(Error::Ended),
            State::Up(_) | State::Parked => {}
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
        // Held from here until its machine has ended, against every other machine, removal
        // and copy of it, in this process or another: taken once, as a lock let go of is held
        // on by a process spawned meanwhile until it execs. Held a moment, waited for; held
        // longer, as by a machine another server runs it in, refused.
        let locking = disk.clone();
        let mut lock =
            tokio::task::spawn_blocking(move || locking.lock_within(DISK_WAIT)).await??;
        // Grown since it last booted: made whole, which holds it itself, and its filesystem
        // written again as large.
        let wanted = self.record.disk_size();
        if disk.capacity()? < wanted {
            drop(lock);
            shared.hooks.materialize(&id, &disk.path).await?;
            let growing = disk.clone();
            lock = tokio::task::spawn_blocking(move || {
                let lock = growing.lock_within(DISK_WAIT)?;
                growing.grow(wanted, &lock).map(|()| lock)
            })
            .await??;
        }
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
            sandbox: id.to_string(),
            started: SystemTime::now(),
            held: Mutex::new(Some((lock, held))),
            writes,
            up: AtomicBool::new(true),
            starting: Mutex::default(),
            synced: Mutex::default(),
            marked: Mutex::default(),
            rebased: Mutex::default(),
            stdins: Mutex::default(),
            written: Mutex::default(),
            turns: Mutex::default(),
        });
        *self.state.lock().unwrap() = State::Up(Arc::clone(&run));
        super::runs::up(&run.sandbox, run.started);
        self.used();
        let (reading, read) = (Arc::clone(self), Arc::clone(&run));
        std::thread::spawn(move || reading.read(&read));
        if let Some(after) = shared.park_after {
            let (sandbox, parked) = (Arc::downgrade(self), Arc::downgrade(&run));
            tokio::spawn(park_when_idle(sandbox, parked, after));
        }
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

    /// Its machine parked now, unless it is in use ([`busy`](Self::busy)): refused
    /// [`InUse`](Error::InUse) then. Its machine stopped with what the guest wrote on its disk,
    /// and its disk kept, to boot again for what next needs it. Nothing is done to one whose
    /// machine is not up.
    pub async fn park(self: &Arc<Self>) -> Result<(), Error> {
        let _turn = self.turn.lock().await;
        if self.is_deleted() {
            return Err(self.gone());
        }
        if !self.up() {
            return Ok(());
        }
        if self.busy() {
            return Err(Error::InUse(format!(
                "sandbox {} is in use: a job runs in it, or it is reached",
                self.record.id
            )));
        }
        self.halt_turned().await
    }

    /// Its machine parked now, whatever uses it: its jobs ended, as cancelling each does -- what an
    /// embedder does for a sandbox whose owner may use it no more.
    pub async fn park_anyway(self: &Arc<Self>) -> Result<(), Error> {
        let _turn = self.turn.lock().await;
        self.halt_turned().await
    }

    /// Its machine parked if it has been unused for `after`, while `run` is its run.
    async fn park_idle(self: &Arc<Self>, run: &Arc<Run>, after: Duration) {
        let _turn = self.turn.lock().await;
        if !self.is(run) || self.idle().is_none_or(|idle| idle < after) {
            return;
        }
        if let Err(e) = self.halt_turned().await {
            eprintln!("sandbox {}, unused, not parked: {e}", self.record.id);
        }
    }

    /// Its machine stopped if it is up, under its turn: its jobs ended, what the guest wrote
    /// put on its disk, and its disk kept.
    async fn halt_turned(self: &Arc<Self>) -> Result<(), Error> {
        let Some(run) = self.take_run()? else {
            return Ok(());
        };
        let stopping = Arc::clone(self);
        tokio::task::spawn_blocking(move || stopping.halt(&run)).await?;
        self.keep();
        Ok(())
    }

    /// Its run, taken out for its machine to be stopped, the sandbox parked: what it may write
    /// marked changed first, so that it is kept once its machine has ended, not let go of
    /// meanwhile.
    fn take_run(&self) -> Result<Option<Arc<Run>>, Error> {
        let run = {
            let mut state = self.state.lock().unwrap();
            match &*state {
                State::Deleted => return Err(self.gone()),
                State::Parked | State::Exited => return Ok(None),
                // Ended on its own already: what reads it makes it exited.
                State::Up(run) if !run.up.load(Ordering::Relaxed) => return Ok(None),
                State::Up(run) => {
                    let run = Arc::clone(run);
                    *state = State::Parked;
                    run
                }
            }
        };
        if let Err(e) = self.shared.hooks.changed(&run.writes) {
            eprintln!("sandbox {}, parked, not to be kept: {e}", self.record.id);
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

    /// As [`park_anyway`](Self::park_anyway), blocking: not on an async runtime's thread.
    pub(super) fn park_blocking(&self) {
        let _turn = self.turn.blocking_lock();
        if let Ok(Some(run)) = self.take_run() {
            self.halt(&run);
            self.keep();
        }
    }

    /// Its machine stopped, if it runs, and it forgotten here with nothing of it kept -- no record
    /// written, its disk left to whoever undoes its making ([`Sandboxes::create`]): what a
    /// making whose caller went away ends with. What the guest wrote is marked changed, so a
    /// disk kept elsewhere still keeps it.
    ///
    /// [`Sandboxes::create`]: super::Sandboxes::create
    pub(super) async fn abandon(self: &Arc<Self>) {
        let _turn = self.turn.lock().await;
        if let Ok(Some(run)) = self.take_run() {
            let stopping = Arc::clone(self);
            let _ = tokio::task::spawn_blocking(move || stopping.halt(&run)).await;
        }
        self.forgotten();
    }

    /// Deleted: its machine stopped first if it is up, its disk removed -- from wherever else
    /// it is kept first -- and its jobs and its record with it. Its volumes stay.
    pub(super) async fn delete(self: &Arc<Self>) -> Result<(), Error> {
        let _turn = self.turn.lock().await;
        self.halt_turned().await?;
        let id = &self.record.id;
        self.shared.hooks.deleting(id).await?;
        match self.shared.snapshots.remove(id) {
            Ok(()) | Err(SnapshotError::Unknown(_)) => {}
            Err(e) => return Err(e.into()),
        }
        self.forgotten();
        Ok(())
    }

    /// Let go of here, its disk left as it is: its record and its jobs gone, as a delete does,
    /// nothing removed from where its disk is kept. Refused while its machine is up.
    pub(super) async fn forget(&self) -> Result<(), Error> {
        let _turn = self.turn.lock().await;
        match &*self.state.lock().unwrap() {
            State::Deleted => return Err(self.gone()),
            State::Up(_) => {
                return Err(Error::InUse(format!(
                    "sandbox {}'s machine is up here: park it first",
                    self.record.id
                )));
            }
            State::Parked | State::Exited => {}
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

    /// Start `cmd` in it, beside whatever else runs there -- its machine booted first if it is
    /// parked --
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
        let run = self.wake().await?;
        let (waiter, started) = oneshot::channel();
        {
            let mut starting = run.starting.lock().unwrap();
            if !run.up.load(Ordering::Relaxed) {
                return Err(Error::Ended);
            }
            run.machine.exec(cmd.clone(), stdin)?;
            starting.push_back(Starting { cmd, waiter });
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

    /// Write `data` to the stdin of the job `id`, started with one, and close it if `eof`:
    /// answered once it is written, so a job that does not read holds the writer back
    /// ([`Run::write_stdin`]).
    pub async fn write_stdin(&self, id: &str, data: String, eof: bool) -> Result<(), Error> {
        let (pid, running) = self.job(id)?.pid();
        let ended = Error::JobEnded(
            "the job has ended, or its stdin is closed, or it was started without one",
        );
        let Some(run) = self.run().filter(|_| running) else {
            return Err(ended);
        };
        {
            let mut stdins = run.stdins.lock().unwrap();
            if !stdins.contains(id) {
                return Err(ended);
            }
            if eof {
                stdins.remove(id);
            }
        }
        run.write_stdin(pid, data, eof).await
    }

    /// The port key `key` names, if it is one of its own.
    pub(super) fn keyed(&self, key: &str) -> Option<u16> {
        self.record.keys.lock().unwrap().get(key).copied()
    }

    /// Its machine, booted first if it is parked, to ask of its files. The caller holds an
    /// [`Activity`] for as long as it asks, so it is not parked under it.
    pub async fn machine(self: &Arc<Self>) -> Result<Arc<Machine>, Error> {
        Ok(Arc::clone(&self.wake().await?.machine))
    }

    /// A connection to `port` in its machine. Refused while its machine is not up, as a port
    /// nothing listens on is: a parked one is not booted for it.
    pub async fn connect(&self, port: u16) -> io::Result<TcpStream> {
        self.ports.connect(port).await
    }

    /// Something done with it from outside, such as a connection through a port or a request
    /// to its files: it is in use until it drops.
    pub fn activity(self: &Arc<Self>) -> Activity {
        self.active.fetch_add(1, Ordering::Relaxed);
        Activity {
            sandbox: Arc::clone(self),
            used: true,
        }
    }

    /// Whether it is in use now: an [`Activity`] under way, or a job running.
    pub(super) fn busy(&self) -> bool {
        if self.active.load(Ordering::Relaxed) > 0 {
            return true;
        }
        let jobs = self.jobs.lock().unwrap();
        jobs.values().any(|job| job.pid().1)
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

    /// Its disk to grow to `gib` GiB as it next boots -- never shrunk: smaller than it is,
    /// refused. Whether its machine is up, its disk to be grown once it boots again.
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
        Ok(self.up())
    }

    /// The bytes its disk is, or is to be as it next boots.
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

    /// Its machine resized, and its disk grown, as it next boots; its disk never shrunk.
    /// A machine is the size it booted as: one up whose vCPUs or memory change is parked now
    /// if nothing uses it, and refused [`InUse`](Error::InUse) if something does. Answered as
    /// it is to be.
    pub async fn resize(
        self: &Arc<Self>,
        vcpus: Option<u8>,
        memory_mib: Option<u32>,
        disk_gib: Option<u32>,
    ) -> Result<Resources, Error> {
        // Not booted meanwhile, nor parked.
        let _turn = self.turn.lock().await;
        if self.is_deleted() {
            return Err(self.gone());
        }
        let now = self.record.args_now();
        let changes = |asked: Option<u32>, is: Option<u32>| asked.is_some_and(|a| Some(a) != is);
        let machine_changes = changes(vcpus.map(u32::from), now.vcpus.map(u32::from))
            || changes(memory_mib, now.memory_mib);
        let resized = Resized {
            vcpus: vcpus.or(now.vcpus),
            memory_mib: memory_mib.or(now.memory_mib),
        };
        spec_of(&SandboxCreateArgs {
            vcpus: resized.vcpus,
            memory_mib: resized.memory_mib,
            ..now.clone()
        })?;
        if let Some(gib) = disk_gib
            && (u64::from(gib) << 30) < self.disk_now()
        {
            return Err(Error::Invalid(format!(
                "disk_gib: a disk is grown, never shrunk -- it is {} GiB",
                self.disk_now().div_ceil(1 << 30)
            )));
        }
        if machine_changes && self.up() {
            if self.busy() {
                return Err(Error::InUse(format!(
                    "sandbox {} is in use: its vCPUs and memory change once nothing uses it",
                    self.record.id
                )));
            }
            self.halt_turned().await?;
        }
        if let Some(gib) = disk_gib {
            *self.record.disk_gib.lock().unwrap() = Some(gib);
        }
        *self.record.resized.lock().unwrap() = resized;
        self.keep();
        let now = self.record.args_now();
        Ok(Resources {
            vcpus: now.vcpus.unwrap_or(DEFAULT_VCPUS),
            memory_mib: now.memory_mib.unwrap_or(DEFAULT_MEMORY_MIB),
            disk_gib: self.record.disk_size().div_ceil(1 << 30) as u32,
            up: self.up(),
        })
    }

    /// What `run`'s machine says, until it ends: each command's pid, as the job its waiter
    /// gets, each job's output and end, and the answers to what was asked of it.
    fn read(&self, run: &Arc<Run>) {
        let machine = &run.machine;
        let _published = self.ports.publish(machine.published.clone());
        let mut running = HashMap::new();
        while let Some(message) = machine.recv() {
            match message {
                CtrlResp::Started(started) => {
                    let Some(Starting { cmd, waiter }) = run.starting.lock().unwrap().pop_front()
                    else {
                        continue;
                    };
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
                CtrlResp::Exited { id, code } => {
                    // Its writes not yet written never will be: their writers told so.
                    run.written.lock().unwrap().remove(&id);
                    run.turns.lock().unwrap().remove(&id);
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
                CtrlResp::Written { id } => {
                    if let Some(waiting) = run.written.lock().unwrap().get_mut(&id)
                        && let Some(told) = waiting.pop_front()
                    {
                        let _ = told.send(());
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
        run.written.lock().unwrap().clear();
        for job in running.into_values() {
            job.end(128 + SIGKILL);
        }
        // Ended on its own, not parked: exited, for good, its disk kept as it was left.
        let ended = {
            let mut state = self.state.lock().unwrap();
            let ended = matches!(&*state, State::Up(up) if Arc::ptr_eq(up, run));
            if ended {
                *state = State::Exited;
            }
            ended
        };
        if ended {
            *self.record.exited.lock().unwrap() = true;
            if let Err(e) = self.shared.hooks.changed(&run.writes) {
                eprintln!("sandbox {}, ended, not to be kept: {e}", self.record.id);
            }
            eprintln!("sandbox {}'s machine ended on its own", self.record.id);
            self.keep();
        }
        run.release();
    }
}

/// What a sandbox's machine is, as resized: what [`Sandbox::resize`] answers. `up`, whether
/// its machine is up now, the size it booted as until it boots again.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Resources {
    pub vcpus: u8,
    pub memory_mib: u32,
    pub disk_gib: u32,
    pub up: bool,
}

/// Park `sandbox` once it has been unused for `after`, for as long as `run` is its run.
async fn park_when_idle(sandbox: Weak<Sandbox>, run: Weak<Run>, after: Duration) {
    let mut wait = after;
    loop {
        tokio::time::sleep(wait).await;
        let (Some(sandbox), Some(run)) = (sandbox.upgrade(), run.upgrade()) else {
            return;
        };
        if !sandbox.is(&run) {
            return;
        }
        wait = match sandbox.idle() {
            Some(idle) if idle >= after => {
                sandbox.park_idle(&run, after).await;
                after
            }
            Some(idle) => after - idle,
            None => after,
        }
        .max(LOOK_EVERY);
    }
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
/// in use, whatever its jobs do -- and, as it drops, used then, unless it came to nothing
/// ([`Activity::unused`]).
pub struct Activity {
    sandbox: Arc<Sandbox>,
    used: bool,
}

impl Activity {
    pub fn sandbox(&self) -> &Arc<Sandbox> {
        &self.sandbox
    }

    /// Ended as nothing done: refused, the sandbox's machine not up. It is not used by it, so a
    /// request it refused keeps the sandbox no longer from being reclaimed.
    pub fn unused(mut self) {
        self.used = false;
    }
}

impl Drop for Activity {
    fn drop(&mut self) {
        if self.used {
            self.sandbox.used();
        }
        self.sandbox.active.fetch_sub(1, Ordering::Relaxed);
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
