//! `virtx-uvm-guest` -- the agent inside each of virtx-uvm's machines.
//!
//! libkrun's init execs this on the boot root. It opens the channel and reads the root to build
//! ([`root`]) as its first message: the image under the machine's ext4, the shares mounted.
//! Standing in it where the image said, it says the guest is up and runs the commands the host
//! sends side by side, each in a process group of its own, sending what they write a line at a
//! time.
//!
//! Beside them it does what the host asks of the guest's files on the I/O channel, one request
//! at a time. What the host sees is the root as the guest's processes see it, volumes and all,
//! but for the kernel's own filesystems; and a link, a device or a pipe is not
//! there as far as it is told: none is listed or followed at the end of a path, and none is
//! removed.
//!
//! Each channel is a named virtio-console port, not stdin and stdout: those are the guest's
//! console, where the kernel prints, and one kernel warning would corrupt a frame there.
//!
//! Once in the root and before it says the guest is up, it adds to its own environment, which
//! every command inherits, what `/alib` holds for this image ([`alib_env`]): only from in here
//! is the image's root seen.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, Read, Seek as _, SeekFrom, Write as _};
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitCode, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, UNIX_EPOCH};

#[path = "../../virtx-uvm/src/machine/wire.rs"]
mod wire;

use wire::{
    CtrlReq, CtrlResp, IO_PORT_NAME, IoFailure, IoMeta, IoReq, IoResp, MAX_DATA, PORT_NAME, Taken,
};

/// The port's writing half, shared by everything that says something on it, so two messages
/// never interleave.
type Out = Arc<Mutex<File>>;

/// The cgroup every command run here goes under -- each in one of its own within it -- its
/// processes held to [`AGENT_PIDS`] fewer than the kernel's threads or pids, its memory to all
/// but [`AGENT_MEMORY`], and its CPU weighed under one of this agent's threads: so that a
/// command forking without end -- a fork bomb -- leaves this agent the processes, the memory and
/// the CPU it needs to answer the host and to stop, and a command run beside it the CPU of one
/// command, not one of the bomb's thousands of processes. None where the kernel has no cgroup2
/// or no `pids` controller.
static JOBS: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
/// What of the kernel's threads the commands may not take: this agent's.
const AGENT_PIDS: u64 = 256;
/// What of the machine's memory the commands may not take, for this agent and the kernel: an
/// eighth of it, up to this.
const AGENT_MEMORY: u64 = 128 << 20;
/// What of the commands' memory one command may not take, for another to start beside it: a
/// sixteenth of the machine's, up to this. Without it, one that takes all there is leaves every
/// other reclaiming beside it to start.
const JOB_ROOM: u64 = 64 << 20;
/// The most memory one command may take, where the kernel has the `memory` controller.
static JOB_MEMORY: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
/// What of the commands' processes one command may not take, for another to start beside it: an
/// eighth of them, up to this. Without it, one forking without end leaves every other command
/// unable to fork.
const JOB_PIDS_ROOM: u64 = 256;
/// The most processes one command may have.
static JOB_PIDS: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
/// The next command's cgroup.
static NEXT_JOB: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Each running command's cgroup, where commands are held by their memory, and how its memory
/// stood at [`watch_thrashing`]'s last look.
static WATCHED: Mutex<std::collections::BTreeMap<PathBuf, Thrash>> =
    Mutex::new(std::collections::BTreeMap::new());
/// How often [`watch_thrashing`] looks, and how many looks in a row a command may thrash at its
/// memory limit before it is killed.
const THRASH_LOOK: Duration = Duration::from_secs(1);
const THRASH_LOOKS: u32 = 3;
/// What of its memory limit a command out of memory has left for pages reclaim can take, at
/// most: a thirty-second of it. One whose own memory fills its limit has its program's pages
/// alone, a few MiB.
const THRASH_ROOM: u64 = 32;

/// A command's memory as [`watch_thrashing`] last saw it: how often it had hit its limit, how
/// many pages it had read back after letting them go, how many looks in a row it thrashed, and
/// whether it was killed for it.
#[derive(Default)]
struct Thrash {
    max: u64,
    refaults: u64,
    strikes: u32,
    killed: bool,
}

/// Kill each command whose memory has run out, as the kernel's OOM killer kills one whose
/// memory it cannot take back. The kernel kills only where reclaiming gets nowhere, and a
/// command whose own anonymous memory fills its limit still has its program's pages to let go
/// of and read back, again and again, for minutes, without end. One that, between two looks,
/// hit its limit, read back more than a sixteenth of it in pages, and holds all but a
/// [`THRASH_ROOM`]th of it in memory that cannot be reclaimed is out of memory; for
/// [`THRASH_LOOKS`] looks in a row, its cgroup is killed. A command that only reads more files
/// than its limit holds hits it and reads back as much, but holds what it read as file pages,
/// which are reclaimed: it is never killed.
fn watch_thrashing() {
    let Some(&most) = JOB_MEMORY.get() else {
        return;
    };
    // SAFETY: `sysconf` has no preconditions.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(1) as u64;
    let enough = most / page / 16;
    let counted = |read: &str, name: &str| -> u64 {
        read.lines()
            .find_map(|line| line.strip_prefix(name)?.strip_prefix(' ')?.parse().ok())
            .unwrap_or(0)
    };
    let read = |own: &Path, file: &str| fs::read_to_string(own.join(file)).unwrap_or_default();
    loop {
        thread::sleep(THRASH_LOOK);
        for (own, thrash) in WATCHED.lock().unwrap().iter_mut() {
            if thrash.killed {
                continue;
            }
            let max = counted(&read(own, "memory.events"), "max");
            let stat = read(own, "memory.stat");
            let refaults = counted(&stat, "workingset_refault_file");
            // What it holds less what reclaim can take: the file pages on their lists, and the
            // kernel's reclaimable caches. Anonymous memory, with no swap, and tmpfs stay.
            let reclaimable = ["active_file", "inactive_file", "slab_reclaimable"]
                .map(|name| counted(&stat, name))
                .iter()
                .sum::<u64>();
            let current = read(own, "memory.current").trim().parse().unwrap_or(0u64);
            let held = current.saturating_sub(reclaimable);
            let thrashing = max > thrash.max
                && refaults.saturating_sub(thrash.refaults) > enough
                && held >= most - most / THRASH_ROOM;
            thrash.strikes = if thrashing { thrash.strikes + 1 } else { 0 };
            (thrash.max, thrash.refaults) = (max, refaults);
            if thrash.strikes >= THRASH_LOOKS {
                thrash.killed = fs::write(own.join("cgroup.kill"), "1").is_ok();
            }
        }
    }
}

/// The cgroup for commands, made: its `pids.max` the kernel's threads or pids, the fewer, less
/// [`AGENT_PIDS`], and each command's that less [`JOB_PIDS_ROOM`]; its `memory.max` the
/// machine's less [`AGENT_MEMORY`], and each command's that less [`JOB_ROOM`]. What it could not
/// hold them by is said on the console.
fn jobs_cgroup() -> Option<PathBuf> {
    let jobs = held_cgroup();
    let held = |what| jobs.is_some() && held_by(what);
    let missing: Vec<&str> = ["pids", "cpu", "memory"]
        .into_iter()
        .filter(|what| !held(what))
        .collect();
    if !missing.is_empty() {
        eprintln!(
            "{}: commands not held by the cgroup controllers {}",
            env!("CARGO_BIN_NAME"),
            missing.join(", ")
        );
    }
    jobs
}

/// Whether the commands' cgroup holds them by the controller `what`.
fn held_by(what: &str) -> bool {
    std::fs::read_to_string("/sys/fs/cgroup/jobs/cgroup.subtree_control")
        .is_ok_and(|held| held.split_whitespace().any(|c| c == what))
}

/// [`jobs_cgroup`]'s making.
fn held_cgroup() -> Option<PathBuf> {
    let root = std::path::Path::new("/sys/fs/cgroup");
    std::fs::write(root.join("cgroup.subtree_control"), "+pids").ok()?;
    // And the CPU: every command together weighs a tenth of one of this agent's threads, which
    // so answers the host while they take the rest -- all of it, when this agent waits, as it
    // mostly does. Without the controller, the pids alone.
    let weighed = std::fs::write(root.join("cgroup.subtree_control"), "+cpu").is_ok();
    let memory = std::fs::write(root.join("cgroup.subtree_control"), "+memory").is_ok();
    let jobs = root.join("jobs");
    std::fs::create_dir_all(&jobs).ok()?;
    // The kernel's own default for its pids, where it was built with fewer: a machine's
    // processes held by its memory (`threads-max`), not by 4096 whatever its size.
    let _ = std::fs::write("/proc/sys/kernel/pid_max", "32768");
    let read =
        |path: &str| -> Option<u64> { std::fs::read_to_string(path).ok()?.trim().parse().ok() };
    // Whichever runs out first: threads, or pids.
    let threads = read("/proc/sys/kernel/threads-max")?.min(read("/proc/sys/kernel/pid_max")?);
    let most = threads.saturating_sub(AGENT_PIDS).max(64);
    std::fs::write(jobs.join("pids.max"), most.to_string()).ok()?;
    let _ = JOB_PIDS.set(most - (most / 8).min(JOB_PIDS_ROOM));
    if weighed {
        let _ = std::fs::write(jobs.join("cpu.weight"), "10");
        let _ = std::fs::write(jobs.join("cgroup.subtree_control"), "+cpu");
    }
    let _ = std::fs::write(jobs.join("cgroup.subtree_control"), "+pids");
    if memory && let Some(total) = memory_total() {
        let together = total - (total / 8).min(AGENT_MEMORY);
        let limited = std::fs::write(jobs.join("memory.max"), together.to_string()).is_ok();
        if limited && std::fs::write(jobs.join("cgroup.subtree_control"), "+memory").is_ok() {
            let _ = JOB_MEMORY.set(together - (total / 16).min(JOB_ROOM));
        }
    }
    Some(jobs)
}

/// Whether the kernel's OOM killer has killed a process in the cgroup `own`: its
/// `memory.events`' `oom_kill`, which is none where the kernel has no `memory` controller.
fn oom_killed(own: &Path) -> bool {
    let events = std::fs::read_to_string(own.join("memory.events")).unwrap_or_default();
    events.lines().any(|line| {
        line.strip_prefix("oom_kill ")
            .and_then(|n| n.trim().parse::<u64>().ok())
            .is_some_and(|n| n > 0)
    })
}

/// The machine's memory, in bytes: `MemTotal` in `/proc/meminfo`.
fn memory_total() -> Option<u64> {
    let info = std::fs::read_to_string("/proc/meminfo").ok()?;
    let line = info.lines().find(|line| line.starts_with("MemTotal:"))?;
    let kib: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kib << 10)
}

/// `command`'s process put in a cgroup of its own under [`JOBS`] before it starts, if there is
/// one: what is answered is that cgroup's `cgroup.procs`, to be held open until the process has
/// started, and the cgroup, to remove once it ends. Those of commands ended that something they
/// left running held then, with nothing left in them now, are removed first.
fn in_jobs(command: &mut Command) -> Option<(File, PathBuf)> {
    let jobs = JOBS.get()?.as_ref()?;
    if let Ok(entries) = std::fs::read_dir(jobs) {
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                // Refused for one with a process in it still.
                let _ = std::fs::remove_dir(entry.path());
            }
        }
    }
    let n = NEXT_JOB.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let own = jobs.join(n.to_string());
    std::fs::create_dir(&own).ok()?;
    if let Some(most) = JOB_MEMORY.get() {
        let _ = std::fs::write(own.join("memory.max"), most.to_string());
    }
    if let Some(most) = JOB_PIDS.get() {
        let _ = std::fs::write(own.join("pids.max"), most.to_string());
    }
    let procs = File::options()
        .write(true)
        .open(own.join("cgroup.procs"))
        .ok()?;
    let fd = procs.as_raw_fd();
    // SAFETY: `write` is async-signal-safe; `fd` is open until the process has started. One
    // that fails leaves the process where it is.
    unsafe {
        command.pre_exec(move || {
            libc::write(fd, b"0".as_ptr().cast(), 1);
            Ok(())
        });
    }
    Some((procs, own))
}

/// The stdin of each process started with one, by pid.
type Stdins = Arc<Mutex<HashMap<u32, Input>>>;

/// How many writes to one process's stdin are queued at most. The host sends the next only once
/// the last is [`Written`](CtrlResp::Written), so one waits at most; past this, as when writes
/// were given up on while the process did not read, one is answered [`Full`](Taken::Full) and
/// not taken, rather than the loop waiting.
const INPUT_QUEUED: usize = 4;

/// What one write to a process's stdin asks: `data`, then its stdin closed if `eof`. `seq` is
/// the host's number for it, which its [`Written`](CtrlResp::Written) carries back.
struct Queued {
    seq: u64,
    data: String,
    eof: bool,
}

/// A process's stdin, written on a thread of its own, each write answered
/// [`Written`](CtrlResp::Written) once it is, or once it never will be: the loop that takes the
/// host's requests never waits on a process's reading, and what a process has not read is never
/// more than [`INPUT_QUEUED`] writes. Dropped, what is queued is still written, and then its
/// stdin closed.
struct Input(std::sync::mpsc::SyncSender<Queued>);

impl Input {
    /// The stdin of process `id`, written to `to`.
    fn start(out: &Out, id: u32, to: ChildStdin) -> Input {
        let (queued, taken) = std::sync::mpsc::sync_channel::<Queued>(INPUT_QUEUED);
        let out = Arc::clone(out);
        thread::spawn(move || {
            let mut to = Some(to);
            while let Ok(Queued { seq, data, eof }) = taken.recv() {
                let (bytes, whole) = match &mut to {
                    Some(pipe) => write_counted(pipe, data.as_bytes()),
                    None => (0, false),
                };
                // Its reader gone, or its stdin closed: nothing more is written.
                if !whole || eof {
                    to = None;
                }
                let taken = if whole { Taken::Whole } else { Taken::Closed };
                send(
                    &out,
                    &CtrlResp::Written {
                        id,
                        seq,
                        bytes,
                        taken,
                    },
                );
            }
        });
        Input(queued)
    }

    /// `queued` queued, for process `id`, and whether it was: where the queue is full it is
    /// answered [`Full`](Taken::Full), and the stdin left as it was.
    fn put(&self, out: &Out, id: u32, queued: Queued) -> bool {
        let seq = queued.seq;
        match self.0.try_send(queued) {
            Ok(()) => true,
            Err(std::sync::mpsc::TrySendError::Full(_)) => {
                send(out, &not_written(id, seq, Taken::Full));
                false
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                send(out, &not_written(id, seq, Taken::Closed));
                false
            }
        }
    }
}

/// Write `data` to `pipe`: how many bytes of it went in, and whether all did. An empty `data`
/// writes nothing and asks whether the pipe still has a reader, which is what any write would
/// find out.
fn write_counted(pipe: &mut ChildStdin, data: &[u8]) -> (u64, bool) {
    if data.is_empty() {
        let mut poll = libc::pollfd {
            fd: pipe.as_raw_fd(),
            events: libc::POLLOUT,
            revents: 0,
        };
        // SAFETY: one `pollfd` of a descriptor `pipe` holds open, not waited on.
        let polled = unsafe { libc::poll(&mut poll, 1, 0) };
        return (0, polled >= 0 && poll.revents & libc::POLLERR == 0);
    }
    let mut bytes = 0;
    while bytes < data.len() {
        match pipe.write(&data[bytes..]) {
            Ok(0) => break,
            Ok(n) => bytes += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    (bytes as u64, bytes == data.len())
}

/// The answer to write `seq` to process `id` that wrote none of it, `taken` saying why.
fn not_written(id: u32, seq: u64, taken: Taken) -> CtrlResp {
    CtrlResp::Written {
        id,
        seq,
        bytes: 0,
        taken,
    }
}

/// The most of a line one message carries: escaped as JSON, a byte is at most six, well within
/// a frame.
const LINE_LIMIT: u64 = 64 << 10;

/// What the host does not see at the root: the kernel's filesystems, which are no files to
/// read or write.
const HIDDEN: [&str; 3] = ["proc", "sys", "dev"];

/// Where the libraries built against each C library are, and venus's manifest in each.
const ALIB_MUSL: &str = "/alib/musl";
const ALIB_GLIBC: &str = "/alib/glibc";
const ALIB_ICD: &str = "virtio_icd.json";

fn main() -> ExitCode {
    match serve() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let said = format!("{}: {e}", env!("CARGO_BIN_NAME"));
            eprintln!("{said}");
            // And to the kernel's log, which reaches the console libkrun logs on the host: the
            // stderr port libkrun gives this agent goes nowhere.
            // Never created: a `/dev` with no devtmpfs on it yet would keep a file of it.
            let _ = File::options()
                .write(true)
                .open("/dev/kmsg")
                .and_then(|mut kmsg| kmsg.write_all(format!("<3>{said}\n").as_bytes()));
            ExitCode::FAILURE
        }
    }
}

/// Where the two disks and the overlay of them are mounted on the boot root, and the
/// directories on the upper disk the overlay writes to.
const LOWER_AT: &str = "/lower";
const UPPER_AT: &str = "/upper";
const UPPER_DIR: &str = "/upper/upper";
const WORK_DIR: &str = "/upper/work";
const NEW_ROOT: &str = "/newroot";

/// Build the root the commands run in and step into it: the upper disk overlaid on the lower,
/// the kernel's filesystems, the resolver named on the overlay, and over it each share where
/// it was asked.
///
/// libkrun's init stays PID 1 on the boot root it started this from, which is where it tells
/// the host how the machine ended.
/// Seeds the guest's random pool with the host's `seed`, crediting it, and has the kernel's
/// generator take it at once; and sets the wall clock to the host's.
fn renew(asked: &wire::MachineInfo) -> io::Result<()> {
    if !asked.seed.is_empty() {
        // `struct rand_pool_info`: bits credited, bytes, then the bytes.
        let mut info = Vec::with_capacity(8 + asked.seed.len());
        info.extend(((asked.seed.len() * 8) as i32).to_ne_bytes());
        info.extend((asked.seed.len() as i32).to_ne_bytes());
        info.extend(&asked.seed);
        let random = File::options().write(true).open("/dev/random")?;
        // RNDADDENTROPY and RNDRESEEDCRNG, _IOW('R', 0x03, int[2]) and _IO('R', 0x07).
        const RNDADDENTROPY: libc::c_ulong = 0x4008_5203;
        const RNDRESEEDCRNG: libc::c_ulong = 0x5207;
        // SAFETY: `info` is a `rand_pool_info` as long as it says; the second takes nothing.
        unsafe {
            if libc::ioctl(random.as_raw_fd(), RNDADDENTROPY as _, info.as_ptr()) != 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::ioctl(random.as_raw_fd(), RNDRESEEDCRNG as _) != 0 {
                return Err(io::Error::last_os_error());
            }
        }
    }
    // A restored guest may hold what its snapshot's disks' first blocks were: dropped, so the
    // disks are read as they are.
    for device in [&asked.lower_device, &asked.upper_device] {
        if let Ok(disk) = File::open(device) {
            // BLKFLSBUF, _IO(0x12, 97).
            // SAFETY: takes no argument.
            unsafe { libc::ioctl(disk.as_raw_fd(), 0x1261 as _) };
        }
    }
    if let Some((secs, nanos)) = asked.now {
        let now = libc::timespec {
            tv_sec: secs as _,
            tv_nsec: nanos as _,
        };
        // SAFETY: a valid timespec.
        if unsafe { libc::clock_settime(libc::CLOCK_REALTIME, &now) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

fn root(asked: &wire::MachineInfo, spike: &mut dyn FnMut(&str)) -> io::Result<()> {
    // The boot root is shared by the time this runs, and a mount under a shared one cannot be
    // moved.
    mount(None, "/", None, libc::MS_REC | libc::MS_PRIVATE, None)?;
    spike("private");
    // The two disks at once: each mount waits on its own disk's reads.
    std::thread::scope(|s| {
        let lower = s.spawn(|| {
            mount(
                Some(&asked.lower_device),
                LOWER_AT,
                Some(&asked.lower_fstype),
                libc::MS_RDONLY,
                None,
            )
        });
        // Mounted once, with barriers: a remount after, to give them back to one mounted
        // without, syncs what the mount wrote -- a journal commit, waited on, that takes longer
        // than the one flush the mount waits on.
        let upper = mount(
            Some(&asked.upper_device),
            UPPER_AT,
            Some(&asked.upper_fstype),
            0,
            None,
        );
        lower
            .join()
            .map_err(|_| io::Error::other("mounting the lower disk panicked"))??;
        upper
    })?;
    spike("disks");
    fs::create_dir_all(UPPER_DIR)?;
    fs::create_dir_all(WORK_DIR)?;
    let layers = format!("lowerdir={LOWER_AT},upperdir={UPPER_DIR},workdir={WORK_DIR}");
    mount(Some("overlay"), NEW_ROOT, Some("overlay"), 0, Some(&layers))?;
    spike("overlay");

    std::env::set_current_dir(NEW_ROOT)?;
    mount(Some("."), "/", None, libc::MS_MOVE, None)?;
    std::os::unix::fs::chroot(".")?;
    std::env::set_current_dir("/")?;
    spike("chroot");

    let quiet = libc::MS_NOEXEC | libc::MS_NOSUID | libc::MS_RELATIME;
    for (source, at, flags) in [
        ("devtmpfs", "/dev", libc::MS_RELATIME),
        ("proc", "/proc", libc::MS_NODEV | quiet),
        ("sysfs", "/sys", libc::MS_NODEV | quiet),
        ("cgroup2", "/sys/fs/cgroup", libc::MS_NODEV | quiet),
        ("devpts", "/dev/pts", quiet),
        ("tmpfs", "/dev/shm", quiet),
    ] {
        fs::create_dir_all(at)?;
        mount(Some(source), at, Some(source), flags, None)?;
    }
    let _ = std::os::unix::fs::symlink("/proc/self/fd", "/dev/fd");
    mount(None, "/", None, libc::MS_REC | libc::MS_SHARED, None)?;
    spike("mounts");

    // A guest whose network interface cannot be set up, or an image whose `/etc/resolv.conf`
    // cannot be written, reaches nothing or resolves no names, and runs anyway.
    if let Some(network) = &asked.network {
        if let Err(e) = network_interface(network) {
            eprintln!(
                "{}: setting up {NETWORK_INTERFACE}: {e}",
                env!("CARGO_BIN_NAME")
            );
        }
        if let Some(nameserver) = network.nameserver
            && let Err(e) = fs::write("/etc/resolv.conf", format!("nameserver {nameserver}\n"))
        {
            eprintln!("{}: naming the resolver: {e}", env!("CARGO_BIN_NAME"));
        }
    }
    for (tag, at) in &asked.shares {
        fs::create_dir_all(at)?;
        mount(Some(tag), at, Some("virtiofs"), 0, None)?;
    }
    spike("shares");
    std::env::set_current_dir(&asked.workdir)
}

/// The guest's one virtio-net interface, which the kernel names first.
const NETWORK_INTERFACE: &str = "eth0";

/// Give [`NETWORK_INTERFACE`] its v4 and v6 addresses and MTU, bring it up, and route everything
/// off the link through the gateway. Connections to a published port arrive at the guest's
/// loopback over the link, which the kernel takes as martian unless told otherwise
/// (`route_localnet`).
fn network_interface(network: &wire::GuestNetwork) -> io::Result<()> {
    fs::write(
        format!("/proc/sys/net/ipv4/conf/{NETWORK_INTERFACE}/route_localnet"),
        "1",
    )?;
    let ioctl = |fd: libc::c_int, request: libc::c_ulong, arg: *mut libc::c_void| {
        // SAFETY: each `arg` is the struct `request` takes, alive for the call.
        match unsafe { libc::ioctl(fd, request as _, arg) } {
            -1 => Err(io::Error::last_os_error()),
            _ => Ok(()),
        }
    };
    let inet = |ip: std::net::Ipv4Addr| {
        let mut addr: libc::sockaddr = unsafe { std::mem::zeroed() };
        let sin = &mut addr as *mut libc::sockaddr as *mut libc::sockaddr_in;
        // SAFETY: a `sockaddr_in` fits in a `sockaddr`, which is what it is passed as.
        unsafe {
            (*sin).sin_family = libc::AF_INET as libc::sa_family_t;
            (*sin).sin_addr.s_addr = u32::from(ip).to_be();
        }
        addr
    };
    // SAFETY: a socket, closed when `fd` drops.
    let fd = match unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) }
    {
        -1 => return Err(io::Error::last_os_error()),
        fd => unsafe { OwnedFd::from_raw_fd(fd) },
    };
    let fd = fd.as_raw_fd();

    // SAFETY: zero is an empty request, filled in below.
    let mut req: libc::ifreq = unsafe { std::mem::zeroed() };
    for (to, from) in req.ifr_name.iter_mut().zip(NETWORK_INTERFACE.bytes()) {
        *to = from as libc::c_char;
    }
    req.ifr_ifru.ifru_addr = inet(network.address);
    ioctl(fd, libc::SIOCSIFADDR, &mut req as *mut _ as _)?;
    let mask = u32::MAX
        .checked_shl(32 - u32::from(network.prefix))
        .unwrap_or(0);
    req.ifr_ifru.ifru_netmask = inet(mask.into());
    ioctl(fd, libc::SIOCSIFNETMASK, &mut req as *mut _ as _)?;
    req.ifr_ifru.ifru_mtu = network.mtu as libc::c_int;
    ioctl(fd, libc::SIOCSIFMTU, &mut req as *mut _ as _)?;
    ioctl(fd, libc::SIOCGIFFLAGS, &mut req as *mut _ as _)?;
    // SAFETY: `SIOCGIFFLAGS` filled in the flags.
    unsafe { req.ifr_ifru.ifru_flags |= (libc::IFF_UP | libc::IFF_RUNNING) as libc::c_short };
    ioctl(fd, libc::SIOCSIFFLAGS, &mut req as *mut _ as _)?;

    // SAFETY: zero is the default route to nowhere, filled in below.
    let mut route: libc::rtentry = unsafe { std::mem::zeroed() };
    route.rt_dst = inet(std::net::Ipv4Addr::UNSPECIFIED);
    route.rt_genmask = inet(std::net::Ipv4Addr::UNSPECIFIED);
    route.rt_gateway = inet(network.gateway);
    route.rt_flags = libc::RTF_UP | libc::RTF_GATEWAY;
    ioctl(fd, libc::SIOCADDRT, &mut route as *mut _ as _)?;

    // v6 is given by hand too, as the gateway sends no router advertisements; duplicate address
    // detection, which would hold the address back a second, finds nothing on a link of two.
    fs::write(
        format!("/proc/sys/net/ipv6/conf/{NETWORK_INTERFACE}/accept_dad"),
        "0",
    )?;
    // SAFETY: a socket, closed when `fd6` drops.
    let fd6 =
        match unsafe { libc::socket(libc::AF_INET6, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) } {
            -1 => return Err(io::Error::last_os_error()),
            fd => unsafe { OwnedFd::from_raw_fd(fd) },
        };
    let fd6 = fd6.as_raw_fd();
    let name = std::ffi::CString::new(NETWORK_INTERFACE).map_err(io::Error::other)?;
    // SAFETY: a NUL-terminated name.
    let index = match unsafe { libc::if_nametoindex(name.as_ptr()) } {
        0 => return Err(io::Error::last_os_error()),
        index => index as libc::c_int,
    };
    let mut address = libc::in6_ifreq {
        ifr6_addr: libc::in6_addr {
            s6_addr: network.address_v6.octets(),
        },
        ifr6_prefixlen: u32::from(network.prefix_v6),
        ifr6_ifindex: index,
    };
    ioctl(fd6, libc::SIOCSIFADDR, &mut address as *mut _ as _)?;
    // The kernel's `struct in6_rtmsg`, whose fields libc keeps private.
    #[repr(C)]
    struct In6Rtmsg {
        dst: libc::in6_addr,
        src: libc::in6_addr,
        gateway: libc::in6_addr,
        kind: u32,
        dst_len: u16,
        src_len: u16,
        metric: u32,
        info: libc::c_ulong,
        flags: u32,
        ifindex: libc::c_int,
    }
    let any = libc::in6_addr { s6_addr: [0; 16] };
    let mut route = In6Rtmsg {
        dst: any,
        src: any,
        gateway: libc::in6_addr {
            s6_addr: network.gateway_v6.octets(),
        },
        kind: 0,
        dst_len: 0,
        src_len: 0,
        metric: 1,
        info: 0,
        flags: u32::from(libc::RTF_UP | libc::RTF_GATEWAY),
        ifindex: index,
    };
    ioctl(fd6, libc::SIOCADDRT, &mut route as *mut _ as _)
}

/// `mount(2)`, its failure naming what was being mounted where.
fn mount(
    source: Option<&str>,
    at: &str,
    fstype: Option<&str>,
    flags: libc::c_ulong,
    data: Option<&str>,
) -> io::Result<()> {
    let c = |s: Option<&str>| {
        s.map(|s| std::ffi::CString::new(s).map_err(io::Error::other))
            .transpose()
    };
    let (source_c, at_c, fstype_c, data_c) =
        (c(source)?, c(Some(at))?.unwrap(), c(fstype)?, c(data)?);
    let ptr = |s: &Option<std::ffi::CString>| s.as_ref().map_or(std::ptr::null(), |s| s.as_ptr());
    // SAFETY: each pointer is null or a string that lives until the call returns.
    let mounted = unsafe {
        libc::mount(
            ptr(&source_c),
            at_c.as_ptr(),
            ptr(&fstype_c),
            flags,
            ptr(&data_c).cast(),
        )
    };
    if mounted == 0 {
        return Ok(());
    }
    let e = io::Error::last_os_error();
    Err(io::Error::new(
        e.kind(),
        format!("mounting {} at {at}: {e}", source.unwrap_or("-")),
    ))
}

/// When the machine has a GPU, put `/alib`'s libraries for the image's C library on
/// `LD_LIBRARY_PATH`, after whatever the image set, and venus as the Vulkan driver when the
/// image names none. Without a GPU there is no driver to give, and `/alib`'s loader, searched
/// before the image's own directories, would only shadow a loader the image brings, such as
/// lavapipe's. Nothing without an `/alib`.
///
/// # Safety
///
/// Changes this process's environment: only while it has one thread.
unsafe fn alib_env() {
    let gpu = fs::read_dir("/dev/dri").is_ok_and(|entries| {
        entries
            .flatten()
            .any(|entry| entry.file_name().to_string_lossy().starts_with("renderD"))
    });
    if !gpu {
        return;
    }
    // The C library `/bin/sh` runs on, by the dynamic linker it names, rather than whether one
    // is installed: an image on glibc may have musl beside it.
    let musl = fs::read("/bin/sh").is_ok_and(|sh| sh.windows(8).any(|w| w == b"ld-musl-"));
    let dir = Path::new(if musl { ALIB_MUSL } else { ALIB_GLIBC });
    if !dir.is_dir() {
        return;
    }
    let path = match std::env::var("LD_LIBRARY_PATH") {
        Ok(set) if !set.is_empty() => format!("{set}:{}", dir.display()),
        _ => dir.display().to_string(),
    };
    let named = ["VK_DRIVER_FILES", "VK_ICD_FILENAMES"]
        .iter()
        .any(|var| std::env::var_os(var).is_some());
    // SAFETY: the caller's.
    unsafe {
        std::env::set_var("LD_LIBRARY_PATH", path);
        if !named {
            // Both names: loaders built before 1.3.207 read only the second.
            let icd = dir.join(ALIB_ICD);
            std::env::set_var("VK_DRIVER_FILES", &icd);
            std::env::set_var("VK_ICD_FILENAMES", &icd);
        }
    }
}

fn serve() -> io::Result<()> {
    // One descriptor, split in two: the driver allows a single opener per port.
    let mut input = File::options()
        .read(true)
        .write(true)
        .open(find(PORT_NAME)?)?;
    let out: Out = Arc::new(Mutex::new(input.try_clone()?));
    send(&out, &CtrlResp::Waiting);
    let asked = wire::recv(&mut input)?
        .ok_or_else(|| io::Error::other("the channel ended before it named a root"))?;
    // Spike: each step's time, told the host before Ready.
    let mut spike = Vec::new();
    let mut spike_at = std::time::Instant::now();
    let mut spike_step = |name: &str| {
        let now = std::time::Instant::now();
        spike.push(format!(
            "{name}={:.1}",
            (now - spike_at).as_secs_f64() * 1000.0
        ));
        spike_at = now;
    };
    renew(&asked)?;
    spike_step("renew");
    root(&asked, &mut spike_step)?;
    // SAFETY: nothing else runs yet; the threads start below.
    unsafe { alib_env() };
    spike_step("alib");
    let io = File::options()
        .read(true)
        .write(true)
        .open(find(IO_PORT_NAME)?)?;
    thread::spawn(move || {
        if let Err(e) = files(io) {
            eprintln!("{}: the I/O channel: {e}", env!("CARGO_BIN_NAME"));
        }
    });
    spike_step("io");

    JOBS.get_or_init(jobs_cgroup);
    spike_step("cgroup");
    thread::spawn(watch_thrashing);
    spike_step("thrash");
    send(
        &out,
        &CtrlResp::Log {
            level: "WARN".into(),
            target: "spike-guest".into(),
            message: spike.join(" "),
        },
    );
    send(&out, &CtrlResp::Ready(Vec::new()));
    let stdins = Stdins::default();
    while let Some(message) = wire::recv(&mut input)? {
        match message {
            CtrlReq::Exec { cmd, stdin } => exec(&out, &stdins, cmd, stdin),
            // Handed to its writer, never written here: a process not reading holds up no other.
            // Answered `Closed` at once for a process with no stdin here. An `eof` not queued
            // leaves the stdin open.
            CtrlReq::Stdin { id, seq, data, eof } => {
                let mut stdins = stdins.lock().unwrap();
                let queued = match stdins.get(&id) {
                    Some(input) => input.put(&out, id, Queued { seq, data, eof }),
                    None => {
                        send(&out, &not_written(id, seq, Taken::Closed));
                        false
                    }
                };
                if eof && queued {
                    stdins.remove(&id);
                }
            }
            // SAFETY: `kill` takes plain values; a group that is gone is an error it returns.
            CtrlReq::Signal { id, signal } => unsafe {
                libc::kill(-(id as libc::pid_t), signal);
            },
            // As libkrun's init ends the machine once its workload has.
            // SAFETY: neither takes anything but plain values.
            CtrlReq::Shutdown => unsafe {
                libc::sync();
                libc::reboot(libc::RB_AUTOBOOT);
            },
            // On a thread of its own, so the agent answers pings while a sync takes a while.
            CtrlReq::Sync => {
                let out = Arc::clone(&out);
                thread::spawn(move || {
                    // SAFETY: `sync` takes nothing.
                    unsafe { libc::sync() };
                    send(&out, &CtrlResp::Synced);
                });
            }
            CtrlReq::Ping => send(&out, &CtrlResp::Pong),
        }
    }
    Ok(())
}

/// How long a port is waited for. The host adds and names each port once the guest's driver
/// asks, so on a busy host a port can still be unnamed when this agent first looks.
const PORT_WAIT: Duration = Duration::from_secs(10);

/// The virtio-console port named `name`, waited for up to [`PORT_WAIT`].
fn find(name: &str) -> io::Result<PathBuf> {
    find_in(
        Path::new("/dev"),
        Path::new("/sys/class/virtio-ports"),
        name,
        PORT_WAIT,
    )
}

/// The port named `name` in `ports`, sysfs's class of them, as its device under `dev`: looked
/// for again until `wait` has passed.
fn find_in(dev: &Path, ports: &Path, name: &str, wait: Duration) -> io::Result<PathBuf> {
    let until = Instant::now() + wait;
    loop {
        let by_name = dev.join("virtio-ports").join(name);
        if by_name.exists() {
            return Ok(by_name);
        }
        // Absent, or not yet named: the port has not come yet. The class unread is said once
        // the wait is over, as the likelier cause.
        let unread = match fs::read_dir(ports) {
            Ok(entries) => {
                for entry in entries.flatten() {
                    let named = fs::read_to_string(entry.path().join("name")).unwrap_or_default();
                    if named.trim() == name {
                        return Ok(dev.join(entry.file_name()));
                    }
                }
                None
            }
            Err(e) => Some(e),
        };
        if Instant::now() >= until {
            let why = unread
                .map(|e| format!(": {}: {e}", ports.display()))
                .unwrap_or_default();
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("no virtio-console port named `{name}` after {wait:?}{why}"),
            ));
        }
        thread::sleep(Duration::from_millis(5));
    }
}

/// Start `cmd` in a process group of its own, its stdin a pipe if `stdin`, answer the host with
/// its pid -- before anything it writes, which goes after -- and send how it ended once it has.
fn exec(out: &Out, stdins: &Stdins, cmd: Vec<String>, stdin: bool) {
    let mut command = Command::new(&cmd[0]);
    command
        .args(&cmd[1..])
        .stdin(if stdin { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let cgroup = in_jobs(&mut command);
    let started = command.spawn();
    // Its process is in it, or never will be: the descriptor that put it there is done.
    let cgroup = cgroup.map(|(_, own)| own);
    let mut child = match started {
        Ok(child) => child,
        Err(e) => {
            if let Some(own) = &cgroup {
                let _ = fs::remove_dir(own);
            }
            send(out, &CtrlResp::Started(Err(format!("{}: {e}", cmd[0]))));
            return;
        }
    };
    let id = child.id();
    if let Some(own) = cgroup.as_ref().filter(|_| JOB_MEMORY.get().is_some()) {
        WATCHED
            .lock()
            .unwrap()
            .insert(own.clone(), Thrash::default());
    }
    // Its stdin taken before its start is said, so that a write the host sends at once finds it.
    if let Some(pipe) = child.stdin.take() {
        stdins
            .lock()
            .unwrap()
            .insert(id, Input::start(out, id, pipe));
    }
    send(out, &CtrlResp::Started(Ok(id)));

    let (read, all_read) = std::sync::mpsc::channel();
    let stdout = Piped::start(
        out,
        child.stdout.take().map(OwnedFd::from),
        read.clone(),
        move |line| CtrlResp::Stdout { id, line },
    );
    let stderr = Piped::start(
        out,
        child.stderr.take().map(OwnedFd::from),
        read,
        move |line| CtrlResp::Stderr { id, line },
    );
    let (out, stdins) = (Arc::clone(out), Arc::clone(stdins));
    thread::spawn(move || {
        // It ends when its process does -- not when its output does, which a program it left
        // running in the background (`server &`) holds open for as long as that runs. What it
        // wrote before it ended is said first, so its end is the last thing said of it: each
        // pipe read up to what was in it as the process ended, or to its end if sooner.
        let code = code(&mut child);
        // Killed, and the kernel's OOM killer killed in its cgroup, or it was killed for
        // thrashing at its limit: its memory ran out. Read before the cgroup goes.
        let starved = cgroup
            .as_ref()
            .and_then(|own| WATCHED.lock().unwrap().remove(own));
        let starved = starved.is_some_and(|thrash| thrash.killed);
        let oom =
            code == 128 + libc::SIGKILL && (starved || cgroup.as_deref().is_some_and(oom_killed));
        // Its cgroup gone with it, unless a program it left running is in it still.
        if let Some(own) = &cgroup {
            let _ = fs::remove_dir(own);
        }
        // Its stdin, if still open, closed once what is queued is: no process reads it.
        drop(stdins.lock().unwrap().remove(&id));
        stdout.exited();
        stderr.exited();
        for _ in 0..2 {
            if all_read.recv().is_err() {
                break;
            }
        }
        send(&out, &CtrlResp::Exited { id, code, oom });
    });
}

/// One of a process's pipes, read on a thread of its own: each line sent up to what was in it
/// when its process ended, and what comes after -- a background program's -- read and dropped,
/// so that a program writing to it is never held up by a full pipe.
struct Piped {
    /// The pipe, held here too so that [`exited`](Piped::exited) asks about it and no
    /// descriptor that took its number after its thread let go.
    pipe: Option<Arc<File>>,
    /// Written a byte by [`exited`](Piped::exited), to wake its thread from waiting on the pipe.
    wake: Option<File>,
    /// What has been read, and once its process has ended, what was in the pipe by then: read
    /// under the lock, so the two are counted alike.
    count: Arc<Mutex<Count>>,
}

#[derive(Default)]
struct Count {
    read: u64,
    until: Option<u64>,
}

impl Piped {
    fn start(
        out: &Out,
        pipe: Option<OwnedFd>,
        read: std::sync::mpsc::Sender<()>,
        wrapped: impl Fn(String) -> CtrlResp + Send + 'static,
    ) -> Self {
        let count = Arc::new(Mutex::new(Count::default()));
        let Some(pipe) = pipe else {
            let _ = read.send(());
            return Self {
                pipe: None,
                wake: None,
                count,
            };
        };
        let (mut woken, wake) = wake_pipe().map_or((None, None), |(r, w)| (Some(r), Some(w)));
        let pipe = Arc::new(File::from(pipe));
        let (out, counted, reading) = (Arc::clone(out), Arc::clone(&count), Arc::clone(&pipe));
        thread::spawn(move || {
            // Each read's lines said together, in as few writes as the device takes: each write
            // costs the machine an exit to its host, dear under nested virtualization.
            let say = |lines: &mut Vec<Vec<u8>>| {
                if lines.is_empty() {
                    return;
                }
                let messages: Vec<CtrlResp> = lines
                    .iter()
                    .map(|line| wrapped(String::from_utf8_lossy(line).into_owned()))
                    .collect();
                let _ = wire::send_all(&mut *out.lock().unwrap(), &messages);
                lines.clear();
            };
            let mut said: Vec<Vec<u8>> = Vec::new();
            let mut line = Vec::new();
            let mut chunk = vec![0u8; 64 << 10];
            let mut told = Some(read);
            loop {
                let caught_up = {
                    let count = counted.lock().unwrap();
                    count.until.is_some_and(|until| count.read >= until)
                };
                if caught_up && let Some(told) = told.take() {
                    if !line.is_empty() {
                        said.push(std::mem::take(&mut line));
                    }
                    say(&mut said);
                    let _ = told.send(());
                    // Nothing waits on the wake past this: its descriptor given back.
                    woken = None;
                }
                // Until its process has ended, the pipe or the wake, whichever comes first; after,
                // the pipe alone, read only to be dropped.
                if told.is_some() && !readable(reading.as_raw_fd(), woken.as_ref()) {
                    continue;
                }
                // How much was read, and how much of it was in the pipe when its process ended.
                let (n, own) = {
                    let mut count = counted.lock().unwrap();
                    match (&*reading).read(&mut chunk) {
                        Ok(0) | Err(_) => (0, 0),
                        Ok(n) => {
                            let before = count.read;
                            count.read += n as u64;
                            let own = count.until.map_or(n as u64, |until| {
                                until.saturating_sub(before).min(n as u64)
                            });
                            (n, own as usize)
                        }
                    }
                };
                if n == 0 {
                    if let Some(told) = told {
                        if !line.is_empty() {
                            said.push(std::mem::take(&mut line));
                        }
                        say(&mut said);
                        let _ = told.send(());
                    }
                    return;
                }
                // Past its process's end, it is a background program's: dropped.
                if told.is_none() {
                    continue;
                }
                let mut rest = &chunk[..own];
                while let Some(at) = rest.iter().position(|b| *b == b'\n') {
                    line.extend_from_slice(&rest[..at]);
                    said.push(std::mem::take(&mut line));
                    rest = &rest[at + 1..];
                }
                line.extend_from_slice(rest);
                // One longer than [`LINE_LIMIT`] goes as several.
                while line.len() as u64 >= LINE_LIMIT {
                    said.push(line.drain(..LINE_LIMIT as usize).collect());
                }
                say(&mut said);
            }
        });
        Self {
            pipe: Some(pipe),
            wake,
            count,
        }
    }

    /// Its process has ended: what is in the pipe now is the last of what it wrote, as far as
    /// the agent can tell -- a program it left running may have written there since it ended.
    fn exited(&self) {
        let mut count = self.count.lock().unwrap();
        let waiting = self.pipe.as_ref().map_or(0, |pipe| {
            let mut n: libc::c_int = 0;
            // SAFETY: a descriptor `pipe` holds open, and an int.
            match unsafe { libc::ioctl(pipe.as_raw_fd(), libc::FIONREAD, &mut n) } {
                0 => n.max(0) as u64,
                _ => 0,
            }
        });
        count.until = Some(count.read + waiting);
        if let Some(mut wake) = self.wake.as_ref() {
            let _ = wake.write_all(&[0]);
        }
    }
}

/// A pipe to wake a [`Piped`]'s thread with: its reading end, then its writing end, neither
/// inherited by a command.
fn wake_pipe() -> io::Result<(File, File)> {
    let mut fds = [0; 2];
    // SAFETY: two ints for `pipe2` to fill.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: two descriptors just made, owned by nothing else.
    Ok(unsafe { (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) })
}

/// Waits for `fd` to have something to read, or to have ended, or for `woken` to be: whether
/// `fd` is ready. What woke `woken` is taken from it. Without one -- the guest out of
/// descriptors -- a tenth of a second at most, so that its process ending is still seen.
fn readable(fd: i32, woken: Option<&File>) -> bool {
    let pollfd = |fd| libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let mut polled = [pollfd(fd), pollfd(woken.map_or(-1, |w| w.as_raw_fd()))];
    let bound = if woken.is_some() { -1 } else { 100 };
    // SAFETY: two `pollfd`s of descriptors held open, or of -1, which `poll` passes over.
    if unsafe { libc::poll(polled.as_mut_ptr(), 2, bound) } <= 0 {
        return false;
    }
    if polled[1].revents != 0
        && let Some(mut woken) = woken
    {
        let _ = woken.read(&mut [0; 8]);
    }
    polled[0].revents != 0
}

/// How `child` ended: its exit code, `128 + signal` for one a signal ended.
fn code(child: &mut Child) -> i32 {
    match child.wait() {
        Ok(status) => status
            .code()
            .or(status.signal().map(|signal| 128 + signal))
            .unwrap_or(-1),
        Err(_) => -1,
    }
}

/// Tell the host `message`. A port that will not take it is a host that is gone, which hears
/// nothing more either way.
fn send(out: &Out, message: &CtrlResp) {
    let _ = wire::send(&mut *out.lock().unwrap(), message);
}

/// Do what the host asks of the files on `port`, each request answered before the next is read,
/// until it stops asking.
fn files(mut port: File) -> io::Result<()> {
    while let Some(request) = wire::recv::<IoReq>(&mut port)? {
        let data = match request {
            IoReq::Write { .. } => wire::recv_data(&mut port)?,
            _ => Vec::new(),
        };
        let mut read = Vec::new();
        let answer = file_io(request, &data, &mut read).unwrap_or_else(IoResp::Failed);
        wire::send(&mut port, &answer)?;
        if answer == IoResp::Data {
            wire::send_data(&mut port, &read)?;
        }
    }
    Ok(())
}

/// Do `request`, with `data` to write, and put what it read in `read`.
fn file_io(request: IoReq, data: &[u8], read: &mut Vec<u8>) -> Result<IoResp, IoFailure> {
    match request {
        IoReq::Stat { path } => {
            let meta = fs::symlink_metadata(shown(&path)?).map_err(failed)?;
            described(&meta)
                .map(IoResp::Meta)
                .ok_or(IoFailure::NotFound)
        }
        IoReq::ReadDir { path } => {
            let path = shown(&path)?;
            let at_root = path == Path::new("/");
            let mut entries = Vec::new();
            for entry in fs::read_dir(&path).map_err(failed)? {
                let entry = entry.map_err(failed)?;
                let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                    continue;
                };
                if at_root && HIDDEN.contains(&name.as_str()) {
                    continue;
                }
                // Not followed: a link is left out, whatever it points at.
                if let Some(meta) = entry.metadata().ok().as_ref().and_then(described) {
                    entries.push((name, meta));
                }
            }
            Ok(IoResp::Entries(entries))
        }
        IoReq::Read { path, offset, len } => {
            let mut file = open_file(
                &shown(&path)?,
                File::options().read(true),
                IoFailure::NotFound,
            )?;
            file.seek(SeekFrom::Start(offset)).map_err(failed)?;
            file.take(len.min(MAX_DATA as u64))
                .read_to_end(read)
                .map_err(failed)?;
            Ok(IoResp::Data)
        }
        IoReq::Write { path, offset } => {
            let mut file = open_file(
                &shown(&path)?,
                File::options().write(true).create(true),
                IoFailure::Forbidden,
            )?;
            match offset {
                Some(offset) => file.seek(SeekFrom::Start(offset)).map(drop),
                None => file.set_len(0),
            }
            .map_err(failed)?;
            file.write_all(data).map_err(failed)?;
            Ok(IoResp::Done)
        }
        IoReq::CreateDir { path } => {
            fs::create_dir(shown(&path)?).map_err(failed)?;
            Ok(IoResp::Done)
        }
        IoReq::Remove { path } => {
            let path = below_root(&path)?;
            match fs::symlink_metadata(&path).map_err(failed)? {
                meta if meta.is_dir() => fs::remove_dir_all(&path),
                meta if meta.is_file() => fs::remove_file(&path),
                _ => return Err(IoFailure::NotFound),
            }
            .map_err(failed)?;
            Ok(IoResp::Done)
        }
        IoReq::Rename { from, to, keep } => {
            let (from, to) = (below_root(&from)?, below_root(&to)?);
            if described(&fs::symlink_metadata(&from).map_err(failed)?).is_none() {
                return Err(IoFailure::NotFound);
            }
            if keep
                && let Ok(there) = fs::symlink_metadata(&to)
                && there.is_file()
            {
                fs::set_permissions(&from, fs::Permissions::from_mode(there.mode()))
                    .map_err(failed)?;
                std::os::unix::fs::lchown(&from, Some(there.uid()), Some(there.gid()))
                    .map_err(failed)?;
            }
            fs::rename(&from, &to).map_err(failed)?;
            Ok(IoResp::Done)
        }
        IoReq::Copy { from, to } => {
            let mut from = open_file(
                &shown(&from)?,
                File::options().read(true),
                IoFailure::NotFound,
            )?;
            let mut to = open_file(
                &below_root(&to)?,
                File::options().write(true).create(true),
                IoFailure::Forbidden,
            )?;
            let (source, there) = (
                from.metadata().map_err(failed)?,
                to.metadata().map_err(failed)?,
            );
            if (source.dev(), source.ino()) == (there.dev(), there.ino()) {
                return Err(IoFailure::Forbidden);
            }
            to.set_len(0).map_err(failed)?;
            io::copy(&mut from, &mut to).map_err(failed)?;
            to.set_permissions(source.permissions()).map_err(failed)?;
            Ok(IoResp::Done)
        }
    }
}

/// `path` as the guest names it, refused where it leads anywhere the host is not shown: it is
/// absolute, and steps neither up nor into a hidden directory.
fn shown(path: &str) -> Result<PathBuf, IoFailure> {
    let Some(rest) = path.strip_prefix('/') else {
        return Err(IoFailure::Forbidden);
    };
    let mut shown = PathBuf::from("/");
    for (i, part) in rest.split('/').filter(|part| !part.is_empty()).enumerate() {
        if matches!(part, "." | "..") || part.contains('\0') {
            return Err(IoFailure::Forbidden);
        }
        if i == 0 && HIDDEN.contains(&part) {
            return Err(IoFailure::NotFound);
        }
        shown.push(part);
    }
    Ok(shown)
}

/// [`shown`], and not the root, which is neither removed nor moved.
fn below_root(path: &str) -> Result<PathBuf, IoFailure> {
    let path = shown(path)?;
    match path == Path::new("/") {
        true => Err(IoFailure::Forbidden),
        false => Ok(path),
    }
}

/// The file at `path` opened with `options`, not followed, and refused with `other` unless it
/// is a regular one.
///
/// Opened without blocking and only then checked, so a pipe or a device the guest made there
/// is neither waited on nor written: `open` would wait for a pipe's other end for good.
fn open_file(
    path: &Path,
    options: &mut fs::OpenOptions,
    other: IoFailure,
) -> Result<File, IoFailure> {
    let file = options
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|e| match e.raw_os_error() {
            // A pipe with no reader, or a device with nothing behind it.
            Some(libc::ENXIO | libc::ENODEV) => other.clone(),
            _ => failed(e),
        })?;
    let meta = file.metadata().map_err(failed)?;
    if meta.is_dir() {
        return Err(IoFailure::IsADirectory);
    }
    if !meta.is_file() {
        return Err(other);
    }
    // SAFETY: `fcntl` takes the descriptor `file` holds open, and plain flags.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFL, 0) } == -1 {
        return Err(failed(io::Error::last_os_error()));
    }
    Ok(file)
}

/// What `meta` is, if it is a file or a directory.
fn described(meta: &fs::Metadata) -> Option<IoMeta> {
    let kind = meta.file_type();
    if !kind.is_file() && !kind.is_dir() {
        return None;
    }
    Some(IoMeta {
        is_dir: kind.is_dir(),
        len: if kind.is_dir() { 0 } else { meta.len() },
        modified: meta
            .modified()
            .ok()
            .and_then(|at| at.duration_since(UNIX_EPOCH).ok()),
    })
}

/// What failing with `e` is told as. A link at the end of a path, which `O_NOFOLLOW` will not
/// open, is one the host is not shown.
fn failed(e: io::Error) -> IoFailure {
    if e.raw_os_error() == Some(libc::ELOOP) {
        return IoFailure::Forbidden;
    }
    match e.kind() {
        io::ErrorKind::NotFound => IoFailure::NotFound,
        io::ErrorKind::AlreadyExists | io::ErrorKind::DirectoryNotEmpty => IoFailure::Exists,
        io::ErrorKind::NotADirectory => IoFailure::NotADirectory,
        io::ErrorKind::IsADirectory => IoFailure::IsADirectory,
        io::ErrorKind::PermissionDenied
        | io::ErrorKind::ReadOnlyFilesystem
        | io::ErrorKind::CrossesDevices
        | io::ErrorKind::InvalidInput => IoFailure::Forbidden,
        io::ErrorKind::StorageFull | io::ErrorKind::QuotaExceeded => IoFailure::Full,
        _ => IoFailure::Other(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory of the test's own, standing in for `/dev` and sysfs.
    fn scratch(test: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("virtx-guest-{}-{test}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_port_named_after_the_first_look_is_found() {
        let dir = scratch("named-late");
        let (dev, ports) = (dir.join("dev"), dir.join("ports"));
        let naming = {
            let ports = ports.clone();
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(100));
                fs::create_dir_all(ports.join("vport1p1")).unwrap();
                fs::write(ports.join("vport1p1/name"), format!("{PORT_NAME}\n")).unwrap();
            })
        };
        let found = find_in(&dev, &ports, PORT_NAME, Duration::from_secs(5));
        naming.join().unwrap();
        assert_eq!(found.unwrap(), dev.join("vport1p1"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_port_never_named_is_not_found_once_the_wait_is_over() {
        let dir = scratch("never-named");
        let ports = dir.join("ports");
        fs::create_dir_all(ports.join("vport1p1")).unwrap();
        fs::write(ports.join("vport1p1/name"), format!("{IO_PORT_NAME}\n")).unwrap();
        let found = find_in(
            &dir.join("dev"),
            &ports,
            PORT_NAME,
            Duration::from_millis(50),
        );
        assert_eq!(found.unwrap_err().kind(), io::ErrorKind::NotFound);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_class_never_there_is_named_once_the_wait_is_over() {
        let dir = scratch("no-class");
        let ports = dir.join("ports");
        let found = find_in(
            &dir.join("dev"),
            &ports,
            PORT_NAME,
            Duration::from_millis(50),
        );
        let said = found.unwrap_err().to_string();
        assert!(said.contains(&ports.display().to_string()), "{said}");
        fs::remove_dir_all(dir).unwrap();
    }
}
