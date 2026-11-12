//! `virtx-uvm-guest` -- the agent inside each of virtx-uvm's machines.
//!
//! libkrun's init has already built the root -- the image under the machine's ext4, the shares
//! mounted -- and execs this standing where the image said. What is left is to open the
//! channel, say the guest is up, and run the commands the host sends side by side, each in a
//! process group of its own, sending what they write a line at a time -- or, for one on a
//! terminal, as it comes.
//!
//! Beside them it does what the host asks of the guest's files on the I/O channel, one request
//! at a time. What the host sees is the root as the guest's processes see it, volumes and all,
//! but for the kernel's own filesystems and the agent's; and a link, a device or a pipe is not
//! there as far as it is told: none is listed or followed at the end of a path, and none is
//! removed.
//!
//! Each channel is a named virtio-console port, not stdin and stdout: those are the guest's
//! console, where the kernel prints, and one kernel warning would corrupt a frame there.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, Read, Seek as _, SeekFrom, Write as _};
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
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
    CtrlReq, CtrlResp, DIAL_PORT, DIALED, IO_PORT_NAME, IoFailure, IoMeta, IoReq, IoResp, MAX_DATA,
    NOT_DIALED, PORT_NAME,
};

/// The port's writing half, shared by everything that says something on it, so two messages
/// never interleave.
type Out = Arc<Mutex<File>>;

/// The terminal of each process on one, by its pid: what the host types into it, and resizes.
type Terminals = Arc<Mutex<HashMap<u32, File>>>;

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
/// The stdin of each process started with one, by pid: what the host writes, written on a
/// thread of its own, so that a process not reading it holds up nothing else; `None` closes it.
type Stdins = Arc<Mutex<HashMap<u32, std::sync::mpsc::Sender<Option<String>>>>>;

/// The most of a line one message carries: escaped as JSON, a byte is at most six, well within
/// a frame.
const LINE_LIMIT: u64 = 64 << 10;

/// What the host does not see at the root: the kernel's filesystems, which are no files to
/// read or write, and the agent's.
const HIDDEN: [&str; 4] = ["proc", "sys", "dev", ".virtx"];

fn main() -> ExitCode {
    match serve() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // To the console, which the host keeps as the machine's log.
            eprintln!("{}: {e}", env!("CARGO_BIN_NAME"));
            ExitCode::FAILURE
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

    // The host's way to a port a program listened on before it was published: see `dial`.
    thread::spawn(|| {
        if let Err(e) = vsock_serve(DIAL_PORT, dialed) {
            eprintln!("{}: dialing guest ports: {e}", env!("CARGO_BIN_NAME"));
        }
    });
    send(&out, &CtrlResp::Ready(Vec::new()));
    let terminals = Terminals::default();
    let stdins = Stdins::default();
    JOBS.get_or_init(jobs_cgroup);
    while let Some(message) = wire::recv(&mut input)? {
        match message {
            CtrlReq::Exec { cmd, stdin } => exec(&out, &stdins, cmd, stdin),
            CtrlReq::ExecTty { cmd, cols, rows } => exec_tty(&out, &terminals, cmd, cols, rows),
            CtrlReq::Stdin { id, data, eof } => {
                if let Some(terminal) = terminals.lock().unwrap().get_mut(&id) {
                    let _ = terminal.write_all(data.as_bytes());
                } else {
                    let mut stdins = stdins.lock().unwrap();
                    if let Some(writer) = stdins.get(&id) {
                        if !data.is_empty() {
                            let _ = writer.send(Some(data));
                        }
                        if eof {
                            let _ = writer.send(None);
                            stdins.remove(&id);
                        }
                    }
                }
            }
            CtrlReq::Resize { id, cols, rows } => {
                if let Some(terminal) = terminals.lock().unwrap().get(&id) {
                    let size = window(cols, rows);
                    // SAFETY: a pty's descriptor, and a valid winsize.
                    unsafe { libc::ioctl(terminal.as_raw_fd(), libc::TIOCSWINSZ, &size) };
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
            CtrlReq::Publish { .. } | CtrlReq::Mark { .. } | CtrlReq::Rebase { .. } => {}
            // Its terminal's foreground group not its own -- it leads its session, so its
            // group is its pid -- is another it started: a command a shell runs.
            CtrlReq::Foreground { id } => {
                let busy = terminals.lock().unwrap().get(&id).is_some_and(|terminal| {
                    // SAFETY: a pty's descriptor.
                    let group = unsafe { libc::tcgetpgrp(terminal.as_raw_fd()) };
                    group > 0 && group != id as libc::pid_t
                });
                send(&out, &CtrlResp::Foreground { id, busy });
            }
        }
    }
    Ok(())
}

/// Listen on vsock `port` for good, each connection -- the host's -- answered by `each` on a
/// thread of its own.
fn vsock_serve(port: u32, each: fn(File)) -> io::Result<()> {
    // SAFETY: plain values in; the descriptor is checked before it is owned.
    let fd = unsafe { libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a socket just made, owned here alone.
    let listener = unsafe { OwnedFd::from_raw_fd(fd) };
    // SAFETY: a zeroed `sockaddr_vm` is a valid one, filled in after.
    let mut at: libc::sockaddr_vm = unsafe { std::mem::zeroed() };
    at.svm_family = libc::AF_VSOCK as libc::sa_family_t;
    at.svm_port = port;
    at.svm_cid = libc::VMADDR_CID_ANY;
    // SAFETY: a valid address of the size given, on our socket.
    let bound = unsafe {
        libc::bind(
            listener.as_raw_fd(),
            (&at as *const libc::sockaddr_vm).cast(),
            std::mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t,
        )
    };
    // SAFETY: our socket.
    if bound < 0 || unsafe { libc::listen(listener.as_raw_fd(), 64) } < 0 {
        return Err(io::Error::last_os_error());
    }
    loop {
        // SAFETY: our listening socket; the address is not asked for.
        let fd = unsafe {
            libc::accept4(
                listener.as_raw_fd(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                libc::SOCK_CLOEXEC,
            )
        };
        if fd < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        // SAFETY: a connection just accepted, owned by the thread it goes to alone.
        let host = unsafe { File::from_raw_fd(fd) };
        thread::spawn(move || each(host));
    }
}

/// The host's connection `host` to [`DIAL_PORT`], joined to the port it names on the guest's
/// own loopback: it says the port as two big-endian bytes, and is answered [`DIALED`] first --
/// or [`NOT_DIALED`] and closed if nothing listens there.
///
/// For a program that listened before its port was published: its `listen` stayed the guest's,
/// as libkrun was not asked to take it, so the host has nothing of its own to connect to. This
/// is the guest's loopback, where it is. A connection to it tries the guest's own stack first,
/// so it reaches that listener -- and is refused, not carried back to the host, where there is
/// none, the port being published.
fn dialed(mut host: File) {
    let mut port = [0u8; 2];
    if host.read_exact(&mut port).is_err() {
        return;
    }
    let port = u16::from_be_bytes(port);
    let guest = std::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port))
        .or_else(|_| std::net::TcpStream::connect((std::net::Ipv6Addr::LOCALHOST, port)));
    let Ok(guest) = guest else {
        let _ = host.write_all(&[NOT_DIALED]);
        return;
    };
    if host.write_all(&[DIALED]).is_err() {
        return;
    }
    let _ = guest.set_nodelay(true);
    splice(&host, &guest);
}

/// Carry what each of `host` and `guest` says to the other until both are done, on this one
/// thread: a socket here is libkrun's (TSI) as much as the kernel's, and one thread blocked
/// reading it holds it from another writing -- so a program that waits to be spoken to before it
/// answers would wait for ever.
fn splice(host: &File, guest: &std::net::TcpStream) {
    let ends = [host.as_raw_fd(), guest.as_raw_fd()];
    // Whether each is still read from: once it is done, the other is told so.
    let mut reading = [true, true];
    let mut buffer = vec![0u8; 64 * 1024];
    while reading.iter().any(|r| *r) {
        let mut polled = [0, 1].map(|i| libc::pollfd {
            fd: if reading[i] { ends[i] } else { -1 },
            events: libc::POLLIN,
            revents: 0,
        });
        // SAFETY: two `pollfd`s of ours, as many as said.
        if unsafe { libc::poll(polled.as_mut_ptr(), 2, -1) } < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return;
        }
        for i in [0, 1] {
            if polled[i].revents == 0 {
                continue;
            }
            let read = match i {
                0 => (&*host).read(&mut buffer),
                _ => (&*guest).read(&mut buffer),
            };
            let n = match read {
                Ok(0) | Err(_) => {
                    reading[i] = false;
                    // SAFETY: our descriptor; its writing half alone ends.
                    unsafe { libc::shutdown(ends[1 - i], libc::SHUT_WR) };
                    continue;
                }
                Ok(n) => n,
            };
            let wrote = match i {
                0 => (&*guest).write_all(&buffer[..n]),
                _ => (&*host).write_all(&buffer[..n]),
            };
            if wrote.is_err() {
                return;
            }
        }
    }
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
    if let Some(mut pipe) = child.stdin.take() {
        let (writes, written) = std::sync::mpsc::channel::<Option<String>>();
        stdins.lock().unwrap().insert(id, writes);
        thread::spawn(move || {
            // Until `None`, or the process gone: dropped, the pipe closes.
            while let Ok(Some(data)) = written.recv() {
                if pipe.write_all(data.as_bytes()).is_err() {
                    break;
                }
            }
        });
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
        // Its stdin, if still open, closed: no process reads it.
        if let Some(writer) = stdins.lock().unwrap().remove(&id) {
            let _ = writer.send(None);
        }
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

/// Start `cmd` on a new terminal, `cols` by `rows`, leading a session of its own with the
/// terminal as its controlling one; answered as [`exec`] answers, and what it writes sent as it
/// comes. The terminal is kept for the host's typing and resizing until the process ends.
fn exec_tty(out: &Out, terminals: &Terminals, cmd: Vec<String>, cols: u16, rows: u16) {
    let refused = |why: String| send(out, &CtrlResp::Started(Err(why)));
    let (master, slave) = match pty(cols, rows) {
        Ok(ends) => ends,
        Err(e) => return refused(format!("a terminal for {}: {e}", cmd[0])),
    };
    let clone = |fd: &OwnedFd| fd.try_clone().map(Stdio::from);
    let (stdin, stdout, stderr) = match (clone(&slave), clone(&slave), clone(&slave)) {
        (Ok(a), Ok(b), Ok(c)) => (a, b, c),
        _ => return refused(format!("a terminal for {}", cmd[0])),
    };
    let mut command = Command::new(&cmd[0]);
    command
        .args(&cmd[1..])
        .stdin(stdin)
        .stdout(stdout)
        .stderr(stderr);
    // SAFETY: `setsid` and `ioctl` are async-signal-safe.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let _cgroup = in_jobs(&mut command);
    let started = command.spawn();
    // The process's alone now: held here too, the terminal would not end with it.
    drop((command, slave));
    let mut child = match started {
        Ok(child) => child,
        Err(e) => return refused(format!("{}: {e}", cmd[0])),
    };
    let id = child.id();
    let master = File::from(master);
    let typed = match master.try_clone() {
        Ok(typed) => typed,
        Err(e) => {
            let _ = child.kill();
            return refused(format!("a terminal for {}: {e}", cmd[0]));
        }
    };
    terminals.lock().unwrap().insert(id, typed);
    send(out, &CtrlResp::Started(Ok(id)));

    let (out, terminals) = (Arc::clone(out), Arc::clone(terminals));
    thread::spawn(move || {
        let mut master = master;
        let mut buffer = vec![0u8; 16 << 10];
        // Bytes of a character cut by the read before, put in front of the next.
        let mut cut = Vec::new();
        loop {
            // Ended once nothing has the terminal open but this: `EIO`.
            let n = match master.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            cut.extend_from_slice(&buffer[..n]);
            let whole = match std::str::from_utf8(&cut) {
                Ok(_) => cut.len(),
                Err(e) if e.error_len().is_none() => e.valid_up_to(),
                Err(_) => cut.len(),
            };
            let data = String::from_utf8_lossy(&cut[..whole]).into_owned();
            cut.drain(..whole);
            send(&out, &CtrlResp::Output { id, data });
        }
        terminals.lock().unwrap().remove(&id);
        send(
            &out,
            &CtrlResp::Exited {
                id,
                code: code(&mut child),
            },
        );
    });
}

/// A new pty, `cols` by `rows`: its master end, and the end the process is given.
fn pty(cols: u16, rows: u16) -> io::Result<(OwnedFd, OwnedFd)> {
    let (mut master, mut slave) = (-1, -1);
    let size = window(cols, rows);
    // SAFETY: both out-pointers are valid; no name or termios is asked for.
    let opened = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &size,
        )
    };
    if opened == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `openpty` answered two descriptors of its own.
    let ends = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    for fd in [&ends.0, &ends.1] {
        // SAFETY: a descriptor of ours; not left to the processes this starts but as their
        // stdio.
        unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) };
    }
    Ok(ends)
}

fn window(cols: u16, rows: u16) -> libc::winsize {
    libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    }
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
