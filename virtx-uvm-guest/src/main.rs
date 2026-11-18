//! `virtx-uvm-guest` -- the agent inside each of virtx-uvm's machines.
//!
//! libkrun's init has already built the root -- the image under the machine's ext4, the shares
//! mounted -- and execs this standing where the image said. What is left is to open the
//! channel, say the guest is up, and run the commands the host sends side by side, each in a
//! process group of its own, sending what they write a line at a time.
//!
//! Beside them it does what the host asks of the guest's files on the I/O channel, one request
//! at a time. What the host sees is the root as the guest's processes see it, volumes and all,
//! but for the kernel's own filesystems and the agent's; and a link, a device or a pipe is not
//! there as far as it is told: none is listed or followed at the end of a path, and none is
//! removed.
//!
//! Each channel is a named virtio-console port, not stdin and stdout: those are the guest's
//! console, where the kernel prints, and one kernel warning would corrupt a frame there.
//!
//! Before any of it, it adds to its own environment, which every command inherits, what
//! `/alib` holds for this image ([`alib_env`]): only from in here is the image's root seen.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, Read, Seek as _, SeekFrom, Write as _};
use std::os::fd::{AsRawFd as _, OwnedFd};
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::UNIX_EPOCH;

#[path = "../../virtx-uvm/src/machine/wire.rs"]
mod wire;

use wire::{
    CtrlReq, CtrlResp, IO_PORT_NAME, IoFailure, IoMeta, IoReq, IoResp, MAX_DATA, PORT_NAME,
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
/// The next command's cgroup.
static NEXT_JOB: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The cgroup for commands, made: its `pids.max` the kernel's threads or pids, the fewer, less
/// [`AGENT_PIDS`]; its `memory.max` the machine's less [`AGENT_MEMORY`], and each command's that
/// less [`JOB_ROOM`].
fn jobs_cgroup() -> Option<PathBuf> {
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

/// The machine's memory, in bytes: `MemTotal` in `/proc/meminfo`.
fn memory_total() -> Option<u64> {
    let info = std::fs::read_to_string("/proc/meminfo").ok()?;
    let line = info.lines().find(|line| line.starts_with("MemTotal:"))?;
    let kib: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kib << 10)
}

/// `command`'s process put in a cgroup of its own under [`JOBS`] before it starts, if there is
/// one: what is answered is that cgroup's `cgroup.procs`, to be held open until the process has
/// started. Those of commands ended, with nothing left in them, are removed first.
fn in_jobs(command: &mut Command) -> Option<File> {
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
    Some(procs)
}
/// The stdin of each process started with one, by pid: its [`Input`].
type Stdins = Arc<Mutex<HashMap<u32, Input>>>;

/// How many writes to one process's input are queued at most. The host sends the next only once
/// the last is [`Written`](CtrlResp::Written), so one waits at most; past this, one is said
/// written, and dropped, rather than the loop waiting.
const INPUT_QUEUED: usize = 4;

/// What one write to a process's input asks: `data`, then its stdin closed if `eof`.
struct Queued {
    data: String,
    eof: bool,
}

/// A process's stdin, written on a thread of its own, each write said
/// [`Written`](CtrlResp::Written) once it is (issue #159). The loop that takes the host's
/// requests never waits on a process's reading, and what a process has not read is never more
/// than [`INPUT_QUEUED`] writes: the host holds the next back until the last is written.
/// Dropped, what is queued is still written, and then its input closed.
struct Input(std::sync::mpsc::SyncSender<Queued>);

impl Input {
    /// The input of process `id`, written to `to`.
    fn start(out: &Out, id: u32, to: impl io::Write + Send + 'static) -> Input {
        let (queued, taken) = std::sync::mpsc::sync_channel::<Queued>(INPUT_QUEUED);
        let out = Arc::clone(out);
        thread::spawn(move || {
            let mut to = Some(to);
            while let Ok(Queued { data, eof }) = taken.recv() {
                if let Some(writer) = &mut to
                    && !data.is_empty()
                    && writer.write_all(data.as_bytes()).is_err()
                {
                    // Its reader gone: nothing more is written.
                    to = None;
                }
                if eof {
                    to = None;
                }
                send(&out, &CtrlResp::Written { id });
            }
        });
        Input(queued)
    }

    /// `queued` queued, for process `id`: said written at once -- and dropped -- where the queue
    /// is full or the writer is gone.
    fn put(&self, out: &Out, id: u32, queued: Queued) {
        if let Err(e) = self.0.try_send(queued) {
            if matches!(e, std::sync::mpsc::TrySendError::Full(_)) {
                eprintln!(
                    "{}: process {id}'s input past {INPUT_QUEUED} writes: one dropped",
                    env!("CARGO_BIN_NAME")
                );
            }
            send(out, &CtrlResp::Written { id });
        }
    }
}

/// The most of a line one message carries: escaped as JSON, a byte is at most six, well within
/// a frame.
const LINE_LIMIT: u64 = 64 << 10;

/// What the host does not see at the root: the kernel's filesystems, which are no files to
/// read or write, and the agent's.
const HIDDEN: [&str; 4] = ["proc", "sys", "dev", ".virtx"];

/// Where the libraries built against each C library are, and venus's manifest in each.
const ALIB_MUSL: &str = "/alib/musl";
const ALIB_GLIBC: &str = "/alib/glibc";
const ALIB_ICD: &str = "virtio_icd.json";

fn main() -> ExitCode {
    // SAFETY: nothing else runs yet; the threads start in `serve`.
    unsafe { alib_env() };
    match serve() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // To the console, which the host keeps as the machine's log.
            eprintln!("{}: {e}", env!("CARGO_BIN_NAME"));
            ExitCode::FAILURE
        }
    }
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
    let io = File::options()
        .read(true)
        .write(true)
        .open(find(IO_PORT_NAME)?)?;
    thread::spawn(move || {
        if let Err(e) = files(io) {
            eprintln!("{}: the I/O channel: {e}", env!("CARGO_BIN_NAME"));
        }
    });

    send(&out, &CtrlResp::Ready(Vec::new()));
    let stdins = Stdins::default();
    JOBS.get_or_init(jobs_cgroup);
    while let Some(message) = wire::recv(&mut input)? {
        match message {
            CtrlReq::Exec { cmd, stdin } => exec(&out, &stdins, cmd, stdin),
            // Handed to its writer, never written here: a process not reading holds up no other.
            // Each answered `Written` -- at once, for a process with no input here.
            CtrlReq::Stdin { id, data, eof } => {
                let mut stdins = stdins.lock().unwrap();
                match stdins.get(&id) {
                    Some(input) => input.put(&out, id, Queued { data, eof }),
                    None => send(&out, &CtrlResp::Written { id }),
                }
                if eof {
                    stdins.remove(&id);
                }
            }
            // SAFETY: `kill` takes plain values; a group that is gone is an error it returns.
            CtrlReq::Signal { id, signal } => unsafe {
                libc::kill(-(id as libc::pid_t), signal);
            },
            CtrlReq::Sync => {
                // SAFETY: `sync` takes nothing.
                unsafe { libc::sync() };
                send(&out, &CtrlResp::Synced);
            }
            // As libkrun's init ends the machine once its workload has.
            // SAFETY: neither takes anything but plain values.
            CtrlReq::Shutdown => unsafe {
                libc::sync();
                libc::reboot(libc::RB_AUTOBOOT);
            },
            // The machine's process answers it, and never passes it on.
            CtrlReq::Mark { .. } | CtrlReq::Rebase { .. } => {}
        }
    }
    Ok(())
}

/// The virtio-console port named `name`.
fn find(name: &str) -> io::Result<PathBuf> {
    let by_name = PathBuf::from("/dev/virtio-ports").join(name);
    if by_name.exists() {
        return Ok(by_name);
    }
    for entry in fs::read_dir("/sys/class/virtio-ports")?.flatten() {
        let named = fs::read_to_string(entry.path().join("name")).unwrap_or_default();
        if named.trim() == name {
            return Ok(PathBuf::from("/dev").join(entry.file_name()));
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("no virtio-console port named `{name}`"),
    ))
}

/// Start `cmd` in a process group of its own, answer the host with its pid -- before anything
/// it writes, which goes after -- and send how it ended once it has.
fn exec(out: &Out, stdins: &Stdins, cmd: Vec<String>, stdin: bool) {
    let mut command = Command::new(&cmd[0]);
    command
        .args(&cmd[1..])
        .stdin(if stdin { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let _cgroup = in_jobs(&mut command);
    let started = command.spawn();
    let mut child = match started {
        Ok(child) => child,
        Err(e) => {
            send(out, &CtrlResp::Started(Err(format!("{}: {e}", cmd[0]))));
            return;
        }
    };
    let id = child.id();
    // Its stdin taken before its start is said, so that a write the host sends at once finds it.
    if let Some(pipe) = child.stdin.take() {
        stdins
            .lock()
            .unwrap()
            .insert(id, Input::start(out, id, pipe));
    }
    send(out, &CtrlResp::Started(Ok(id)));

    let ended = Arc::new(Mutex::new(false));
    let (read, all_read) = std::sync::mpsc::channel();
    let stdout = Piped::start(
        out,
        child.stdout.take().map(OwnedFd::from),
        &ended,
        read.clone(),
        move |line| CtrlResp::Stdout { id, line },
    );
    let stderr = Piped::start(
        out,
        child.stderr.take().map(OwnedFd::from),
        &ended,
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
        // Its stdin, if still open, closed once what is queued is: no process reads it.
        drop(stdins.lock().unwrap().remove(&id));
        stdout.exited();
        stderr.exited();
        for _ in 0..2 {
            if all_read.recv().is_err() {
                break;
            }
        }
        let mut ended = ended.lock().unwrap();
        *ended = true;
        send(&out, &CtrlResp::Exited { id, code });
    });
}

/// One of a process's pipes, read on a thread of its own: each line sent until its process has
/// ended and what it wrote by then is read, and what comes after -- a background program's --
/// read and dropped, so that a program writing to it is never held up by a full pipe.
struct Piped {
    fd: Option<i32>,
    /// What has been read, and once its process has ended, what it had written by then:
    /// read under the lock, so the two are counted alike.
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
        ended: &Arc<Mutex<bool>>,
        read: std::sync::mpsc::Sender<()>,
        wrapped: impl Fn(String) -> CtrlResp + Send + 'static,
    ) -> Self {
        let count = Arc::new(Mutex::new(Count::default()));
        let Some(pipe) = pipe else {
            let _ = read.send(());
            return Self { fd: None, count };
        };
        let fd = pipe.as_raw_fd();
        let (out, ended, counted) = (Arc::clone(out), Arc::clone(ended), Arc::clone(&count));
        thread::spawn(move || {
            let pipe = File::from(pipe);
            // Each read's lines said at once, one write for them all: each write costs the
            // machine an exit to its host, dear under nested virtualization.
            let say = |lines: &mut Vec<Vec<u8>>| {
                if lines.is_empty() {
                    return;
                }
                // Under `ended`, so nothing is said of a process after its end.
                let ended = ended.lock().unwrap();
                if !*ended {
                    let messages: Vec<CtrlResp> = lines
                        .iter()
                        .map(|line| wrapped(String::from_utf8_lossy(line).into_owned()))
                        .collect();
                    let _ = wire::send_all(&mut *out.lock().unwrap(), &messages);
                }
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
                }
                // A while at most, so that its process ending is seen while nothing comes.
                if !readable(fd, 100) {
                    continue;
                }
                let n = {
                    let mut count = counted.lock().unwrap();
                    match (&pipe).read(&mut chunk) {
                        Ok(0) | Err(_) => 0,
                        Ok(n) => {
                            count.read += n as u64;
                            n
                        }
                    }
                };
                if n == 0 {
                    if !line.is_empty() {
                        said.push(std::mem::take(&mut line));
                    }
                    say(&mut said);
                    if let Some(told) = told {
                        let _ = told.send(());
                    }
                    return;
                }
                let mut rest = &chunk[..n];
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
            fd: Some(fd),
            count,
        }
    }

    /// Its process has ended: what is in the pipe now is the last of what it wrote.
    fn exited(&self) {
        let mut count = self.count.lock().unwrap();
        let waiting = self.fd.map_or(0, |fd| {
            let mut n: libc::c_int = 0;
            // SAFETY: a descriptor its thread holds open until the pipe ends, and an int.
            match unsafe { libc::ioctl(fd, libc::FIONREAD, &mut n) } {
                0 => n.max(0) as u64,
                _ => 0,
            }
        });
        count.until = Some(count.read + waiting);
    }
}

/// Whether `fd` has something to read, or has ended, within `ms` milliseconds.
fn readable(fd: i32, ms: i32) -> bool {
    let mut polled = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one `pollfd` of ours.
    unsafe { libc::poll(&mut polled, 1, ms) > 0 }
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
