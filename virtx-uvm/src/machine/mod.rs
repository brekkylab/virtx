//! A machine: a root filesystem booted with a writable layer over it, running the guest, which
//! runs the commands it is sent.
//!
//! A machine is a process of its own. Booting one makes the process that does it into the
//! machine, which does not come back: the process ends when the guest does. So
//! [`Machine::run`] spawns the binary it is given, as `<exe> vm <spawned>`, to be the machine --
//! usually the one it is in, again. On macOS that is also what the hypervisor entitlement needs, since it
//! is read from the signature at `exec`. That binary's `main` hands [`SUBCOMMAND`] to [`enter`]
//! before anything else, and [`enter`] builds the machine and becomes it (see `boot`).
//!
//! A machine runs one command: the guest's init runs it, and the machine ends when it does,
//! with its exit code. The command's stdin, stdout and stderr are the machine process's own,
//! which libkrun connects the guest's to directly. Beside them is a control channel of two
//! pipes (see [`wire`]): `ctlin`, which the host stops the machine through, and
//! `ctlout`, which the process says everything else on -- where its ports are published,
//! what the guest's console and libkrun log, and why the machine could not be built.

use std::collections::VecDeque;
use std::convert::Infallible;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use oci_spec::image::{Config, Digest};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt as _, AsyncRead, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::time::Instant;

use wire::{HostMessage, MachineMessage};

use crate::fstree;
use crate::snapshot::{self, Snapshot};
use crate::volume;

mod boot;
#[cfg(feature = "gpu")]
mod gpu;
mod net;
pub mod wire;

pub use net::Network;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A temporary snapshot that could not be formatted.
    #[error(transparent)]
    Fstree(#[from] fstree::Error),

    /// The snapshot could not be held for this machine: another one runs over it.
    #[error(transparent)]
    Snapshot(#[from] snapshot::Error),

    /// A volume that could not be opened or held for this machine, or a store in it that
    /// could not be made.
    #[error(transparent)]
    Volume(#[from] volume::Error),

    /// A volume the guest could not be shown where it was asked: somewhere not absolute, the
    /// root, or where another volume already is.
    #[error("volume {} at {at}: {why}", path.display())]
    MountPoint {
        path: PathBuf,
        at: String,
        why: &'static str,
    },

    /// A port that cannot be published: `0`, or one given twice.
    #[error("port {port}: {why}")]
    Port { port: u16, why: &'static str },

    /// A device, a payload or a machine libkrun would not build. What went wrong is
    /// libkrun's to say; what it was doing is this end's.
    #[error("{context}: {source}")]
    Vmm {
        context: String,
        #[source]
        source: krun::VmmError,
    },

    /// The guest init could not be laid over the root, which is a machine with no PID 1.
    #[error("preparing the guest init: {0}")]
    Init(#[source] krun_init::ApplyError),

    /// The machine started and its event loop then broke -- the only way [`enter`] comes back
    /// once the machine is built.
    ///
    /// No source, because there is none to carry: libkrun logs what went wrong and returns
    /// nothing.
    #[error("the machine's event loop stopped; libkrun logged why")]
    EventLoop,

    /// A machine given no command, which would have nothing to run.
    #[error("a machine runs a command, and none was given")]
    NoCommand,

    /// The machine's process could not build the machine, or it broke: why, with the end of
    /// what the guest's console and libkrun said.
    #[error("{0}")]
    DidNotStart(String),

    /// The machine's process ended without a command's exit code -- killed, or crashed: how,
    /// with the end of what the guest's console and libkrun said.
    #[error("{0}")]
    Died(String),

    #[error("the command ran past its timeout of {0}ms and the machine was stopped")]
    TimedOut(u64),

    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },
}

impl Error {
    fn vmm(context: impl Into<String>, source: krun::VmmError) -> Error {
        Error::Vmm {
            context: context.into(),
            source,
        }
    }

    fn io(context: impl Into<String>, source: std::io::Error) -> Error {
        Error::Io {
            context: context.into(),
            source,
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// The subcommand a machine's process is spawned with, which the binary's `main` hands to
/// [`enter`].
pub const SUBCOMMAND: &str = "vm";

/// How long a machine told to stop has to end before it is killed.
const STOP_GRACE: Duration = Duration::from_secs(5);

/// How many of the last lines the guest's console and libkrun said a failure is told with.
const TAIL: usize = 50;

/// What a machine is: the image it boots, the image's config it runs under, where its writes
/// go, and how big a machine runs them.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Machine {
    /// The image, as the store names it.
    pub image: Digest,

    /// Its root filesystem's disk, as [`Store::insert_rootfs`](crate::ImageStore::insert_rootfs)
    /// answers it.
    pub rootfs: PathBuf,

    /// The image's runtime config: the environment and working directory the guest, and every
    /// command it runs, is given. Its entrypoint and command are not run.
    pub config: Config,

    /// Where the machine's writes go. `None` is a temporary one, gone with the machine.
    pub snapshot: Option<Snapshot>,

    pub vcpus: u8,

    pub ram_mib: u32,

    /// A virtio-gpu device carrying Vulkan, by way of venus. **Ignored unless the crate was
    /// built with the `gpu` feature**: a build that cannot serve Vulkan runs the machine
    /// without it rather than refusing to start. The image has to carry the guest half.
    pub gpu: bool,

    /// How much memory the guest's GPU may hold, in mebibytes. `None` is libkrun's 8 GiB.
    /// Ignored without [`gpu`](Machine::gpu).
    pub gpu_memory_mib: Option<u32>,

    /// The volumes the guest sees, in the order they are mounted.
    #[serde(default)]
    pub volumes: Vec<VolumeMount>,

    /// Where the guest's own connections may go: the way out.
    #[serde(default)]
    pub network: Network,

    /// TCP ports in the guest the machine's process reaches, each on its loopback at a port
    /// of its choosing: the way in, which does not need a
    /// [`network`](Machine::network).
    #[serde(default)]
    pub ports: Vec<u16>,
}

/// A volume shown in the guest. Pointed at rather than carried, so its mounts' credentials stay
/// in its `mounts.json` and out of the config.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeMount {
    /// Its own directory, as [`Volume::path`](crate::Volume::path) answers it.
    pub path: PathBuf,

    /// Where it appears in the guest: absolute, and made there if it is not.
    pub at: String,

    /// Whether the host refuses the guest's writes to its files. Its stores take none either
    /// way.
    pub read_only: bool,
}

/// What a machine reports while it runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Report {
    /// A line the command wrote to its stdout, without its newline.
    Stdout(String),
    /// A line the command wrote to its stderr, without its newline.
    Stderr(String),
    /// Where on this host's loopback each of [`ports`](Machine::ports) is reached; before
    /// the command runs.
    Published(Vec<(u16, SocketAddr)>),
    /// A line the guest's console printed: the kernel's, or the init's before it ran the
    /// command.
    Console(String),
    /// What libkrun logged of its own.
    Log { level: String, message: String },
}

/// What a machine's process is spawned with, as one argument: the machine, its command, and
/// its ends of the control channel.
#[derive(Serialize, Deserialize)]
struct Spawned {
    machine: Machine,
    cmd: Vec<String>,
    ctlin: u64,
    ctlout: u64,
}

impl Machine {
    /// Boot this machine in a process of its own, run `cmd` in it, and answer how the command
    /// ended once the machine has: its exit code, `128 + signal` for one a signal ended. What
    /// the machine reports meanwhile goes to `report`.
    ///
    /// Past `timeout_ms` the machine is stopped, and killed if it does not end. Dropped before
    /// it has ended, it is killed. Either way what the guest had not yet written to its disk is
    /// lost.
    ///
    /// The process is `exe`, which has to hand [`SUBCOMMAND`] to [`enter`] first thing in its
    /// `main`, and on macOS be signed with the `com.apple.security.hypervisor` entitlement.
    pub async fn run(
        self,
        exe: impl AsRef<Path>,
        cmd: Vec<String>,
        timeout_ms: Option<u64>,
        mut report: impl FnMut(Report),
    ) -> Result<i32> {
        if cmd.is_empty() {
            return Err(Error::NoCommand);
        }
        let (ctlin, mut stop) = io::pipe().map_err(|e| Error::io("making ctlin", e))?;
        let (mut said, ctlout) = io::pipe().map_err(|e| Error::io("making ctlout", e))?;
        let spawned = Spawned {
            machine: self,
            cmd,
            ctlin: raw(&ctlin),
            ctlout: raw(&ctlout),
        };
        let mut command = Command::new(exe.as_ref());
        command
            .arg(SUBCOMMAND)
            .arg(serde_json::to_string(&spawned).expect("a machine's spawn serializes"))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        inherit(&mut command, [spawned.ctlin, spawned.ctlout])
            .map_err(|e| Error::io("handing the machine its control channel", e))?;
        let mut process = command
            .spawn()
            .map_err(|e| Error::io("starting the machine's process", e))?;
        // The process's now: held here too, they would keep the channel open past its end.
        drop((ctlin, ctlout));

        // Everything it reports, from the three places it does, into one place to hand it on from.
        let (reports, mut reported) = mpsc::unbounded_channel();
        let stdout = process.stdout.take().expect("piped");
        tokio::spawn(lines(stdout, reports.clone(), Report::Stdout));
        let stderr = process.stderr.take().expect("piped");
        tokio::spawn(lines(stderr, reports.clone(), Report::Stderr));
        let control = tokio::task::spawn_blocking(move || {
            let mut not_built = None;
            while let Ok(Some(message)) = wire::recv(&mut said) {
                let _ = reports.send(match message {
                    MachineMessage::Published(ports) => Report::Published(ports),
                    MachineMessage::Console(line) => Report::Console(line),
                    MachineMessage::Log { level, message } => Report::Log { level, message },
                    MachineMessage::NotBuilt(why) => {
                        not_built = Some(why);
                        continue;
                    }
                });
            }
            not_built
        });

        // Until all three have ended, which the process ending is. A machine past its timeout is
        // told to stop, and killed once it has had the grace to.
        let mut tail = VecDeque::with_capacity(TAIL);
        let mut deadline = timeout_ms.map(|ms| Instant::now() + Duration::from_millis(ms));
        let mut stopped = false;
        loop {
            tokio::select! {
                got = reported.recv() => {
                    let Some(got) = got else { break };
                    if let Report::Console(line) | Report::Log { message: line, .. } = &got {
                        if tail.len() == TAIL {
                            tail.pop_front();
                        }
                        tail.push_back(line.clone());
                    }
                    report(got);
                }
                () = tokio::time::sleep_until(deadline.unwrap_or_else(Instant::now)), if deadline.is_some() => {
                    deadline = match stopped {
                        false => {
                            let _ = wire::send(&mut stop, &HostMessage::Stop);
                            Some(Instant::now() + STOP_GRACE)
                        }
                        true => {
                            let _ = process.start_kill();
                            None
                        }
                    };
                    stopped = true;
                }
            }
        }
        let not_built = control
            .await
            .map_err(|e| Error::io("reading ctlout", io::Error::other(e)))?;
        let status = process
            .wait()
            .await
            .map_err(|e| Error::io("waiting for the machine's process", e))?;

        let told = |why: String| match tail.is_empty() {
            true => why,
            false => format!("{why}; last said:\n{}", Vec::from(tail.clone()).join("\n")),
        };
        if stopped {
            return Err(Error::TimedOut(timeout_ms.unwrap_or_default()));
        }
        if let Some(why) = not_built {
            return Err(Error::DidNotStart(told(why)));
        }
        match status.code() {
            Some(code) => Ok(code),
            None => Err(Error::Died(told(format!(
                "the machine's process ended with {status}"
            )))),
        }
    }
}

/// A pipe end as the command line names it.
///
/// Neither std nor tokio hands a child anything past its stdin, stdout and stderr, so the
/// control channel's two ends are kept open across `exec` and named on its command line: by
/// descriptor number on Unix, by handle value on Windows.
#[cfg(unix)]
fn raw(end: &impl std::os::fd::AsRawFd) -> u64 {
    end.as_raw_fd() as u64
}

#[cfg(windows)]
fn raw(end: &impl std::os::windows::io::AsRawHandle) -> u64 {
    end.as_raw_handle() as usize as u64
}

/// Keep `ends` open in the process `command` spawns, which they are not by default.
#[cfg(unix)]
fn inherit(command: &mut Command, ends: [u64; 2]) -> io::Result<()> {
    // SAFETY: `fcntl` is async-signal-safe, which is all that may run between `fork` and
    // `exec`.
    unsafe {
        command.pre_exec(move || {
            for end in ends {
                if libc::fcntl(end as i32, libc::F_SETFD, 0) == -1 {
                    return Err(io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    Ok(())
}

/// Keep `ends` open in the process `command` spawns, which they are not by default.
///
/// Inheritable from here on, so a process this one spawns meanwhile gets them too, and holds
/// the channel open for as long as it lives.
#[cfg(windows)]
fn inherit(_: &mut Command, ends: [u64; 2]) -> io::Result<()> {
    use windows_sys::Win32::Foundation::{HANDLE_FLAG_INHERIT, SetHandleInformation};

    for end in ends {
        // SAFETY: `end` is a handle this process holds open until the child is spawned.
        let set = unsafe {
            SetHandleInformation(end as usize as _, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT)
        };
        if set == 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Each line `read` gives, as `wrapped` makes it a report, until it ends.
async fn lines(
    read: impl AsyncRead + Unpin,
    reports: mpsc::UnboundedSender<Report>,
    wrapped: fn(String) -> Report,
) {
    let mut lines = BufReader::new(read).split(b'\n');
    while let Ok(Some(line)) = lines.next_segment().await {
        let _ = reports.send(wrapped(String::from_utf8_lossy(&line).into_owned()));
    }
}

/// Become the machine this process was spawned with by [`Machine::run`]. Does not come back unless the
/// machine could not be built or broke, and either way tells the host why first.
pub fn enter() -> Result<Infallible> {
    let spawned = std::env::args_os().nth(2).unwrap_or_default();
    let spawned: Spawned = serde_json::from_slice(spawned.as_encoded_bytes())
        .map_err(|e| Error::io("reading the machine to be", io::Error::other(e)))?;
    // SAFETY: `run` left these open for this process, and named them to it alone.
    unsafe { boot::hold(spawned.ctlin, spawned.ctlout) };
    let entered = boot::boot(spawned.machine, &spawned.cmd);
    if let Err(e) = &entered {
        let _ = boot::send(&MachineMessage::NotBuilt(e.to_string()));
    }
    entered
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the machine's process is handed is what the parent wrote.
    #[test]
    fn a_config_survives_the_trip_to_the_machine_s_process() {
        let mut config = Config::default();
        config.set_env(Some(vec!["A=1".to_string()]));
        let sent = Machine {
            image: format!("sha256:{}", "a".repeat(64)).parse().unwrap(),
            rootfs: PathBuf::from("/store/rootfs/sha256/abc.vmdk"),
            config,
            snapshot: Some(Snapshot {
                name: "work".to_string(),
                path: PathBuf::from("/snapshot/work.ext4"),
            }),
            vcpus: 2,
            ram_mib: 2048,
            gpu: false,
            gpu_memory_mib: None,
            volumes: vec![VolumeMount {
                path: PathBuf::from("/store/volume/data"),
                at: "/data".to_string(),
                read_only: true,
            }],
            network: Network::Public,
            ports: vec![3000],
        };
        let read: Machine = serde_json::from_str(&serde_json::to_string(&sent).unwrap()).unwrap();
        assert_eq!(read.image, sent.image);
        assert_eq!(read.rootfs, sent.rootfs);
        assert_eq!(read.config, sent.config);
        assert_eq!(read.snapshot, sent.snapshot);
        assert_eq!(read.volumes, sent.volumes);
        assert_eq!(read.network, sent.network);
        assert_eq!(read.ports, sent.ports);
    }
}
