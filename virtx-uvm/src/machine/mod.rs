//! A machine: a root filesystem booted with a writable layer over it, running the guest, which
//! runs the commands it is sent.
//!
//! A machine is a process of its own. Booting one makes the process that does it into the
//! machine, which does not come back: the process ends when the guest does. So
//! [`Machine::start`] spawns the binary it is in again, as `<exe> vm <config> <guest>`, to be the
//! machine -- on macOS that is also what the hypervisor entitlement needs, since it is read from the
//! signature at `exec`. That binary's `main` hands [`SUBCOMMAND`] to [`enter`] before anything
//! else, and [`enter`] builds the machine and becomes it (see `boot`).
//!
//! The machine process's stdin and stdout are the [`wire`] to the guest, and its stderr is the
//! guest's console, which goes to a file beside the guest binary's copy. One task writes the
//! calls into the guest and another reads what comes back, handing each message to the call it
//! answers, so calls run side by side. When the channel ends, every call still waiting hears
//! the machine is gone.

use std::collections::HashMap;
use std::convert::Infallible;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use oci_spec::image::{Config, Digest};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt as _, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

use wire::{FromGuest, HANDSHAKE, ToGuest, recv, send};

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

    /// The machine's process was started and the guest did not come up in it: what is known of
    /// why, with the end of what the console said.
    #[error("{0}")]
    DidNotStart(String),

    /// A command the guest could not start, or not wait for.
    #[error("{0}")]
    NotStarted(String),

    #[error("the command ran past its timeout of {0}ms and was killed")]
    TimedOut(u64),

    /// The machine is down, or its channel is: nothing more will be answered.
    #[error("the machine is gone")]
    Gone,

    #[error("the guest said {0}")]
    Unexpected(String),

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

/// Where the guest binary is shown in the guest, and its name there and beside this binary.
const GUEST_DIR: &str = "/.virtx";
const GUEST_BIN: &str = "virtx-uvm-guest";

/// Names a guest binary to boot instead of the one beside this binary.
const GUEST_BIN_ENV: &str = "VIRTX_UVM_GUEST_BIN";

/// How long the guest has to come up before it is taken for one that will not.
const BOOT_TIMEOUT: Duration = Duration::from_secs(60);

/// What a machine is: the image it boots, the image's config it runs under, where its writes
/// go, and how big a machine runs them.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MachineConfig {
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
    /// Ignored without [`gpu`](MachineConfig::gpu).
    pub gpu_memory_mib: Option<u32>,

    /// The volumes the guest sees, in the order they are mounted.
    #[serde(default)]
    pub volumes: Vec<VolumeMount>,

    /// Where the guest's own connections may go: the way out.
    #[serde(default)]
    pub network: Network,

    /// TCP ports in the guest the machine's process reaches, each on its loopback at a port
    /// of its choosing: the way in, which does not need a
    /// [`network`](MachineConfig::network).
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

/// Become the machine this process was spawned with by [`Machine::start`]: its config, and the
/// directory holding the guest binary, shown read-only at [`GUEST_DIR`]. Does not come back
/// unless the machine could not be built or broke.
pub fn enter() -> Result<Infallible> {
    let mut args = std::env::args_os().skip(2);
    let config = args.next().unwrap_or_default();
    let config = serde_json::from_slice(config.as_encoded_bytes())
        .map_err(|e| Error::io("reading the machine to be", std::io::Error::other(e)))?;
    boot::boot(config, args.next().unwrap_or_default().into())
}

/// Where each call's answers go, by id, until its last; `None` once the channel has ended.
type Calls = Arc<Mutex<Option<HashMap<u64, mpsc::UnboundedSender<FromGuest>>>>>;

/// A running machine, and the guest in it. Dropping it kills the machine's process and removes
/// what was made for it.
pub struct Machine {
    config: MachineConfig,
    process: tokio::sync::Mutex<Child>,
    /// The calls, to the one task that writes them, so two never interleave and one can be
    /// sent from where nothing can wait.
    outgoing: mpsc::UnboundedSender<ToGuest>,
    calls: Calls,
    next: AtomicU64,
    /// How many commands run in it now, and when the last one ended.
    running: AtomicUsize,
    used: Mutex<Instant>,
    reader: tokio::task::JoinHandle<()>,
    writer: tokio::task::JoinHandle<()>,
    /// The guest binary's copy and the console's log, which nothing else reads.
    dir: PathBuf,
}

impl Drop for Machine {
    fn drop(&mut self) {
        self.reader.abort();
        self.writer.abort();
        let _ = self.process.get_mut().start_kill();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Machine {
    /// Boot `config` in a process of its own, and wait for the guest to say it is serving.
    pub async fn start(config: MachineConfig) -> Result<Machine> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "virtx-uvm-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let started = async {
            // A copy and not the binary where it is: the directory is shared with the guest,
            // and should hold nothing else.
            let guest = dir.join("guest");
            std::fs::create_dir_all(&guest)
                .map_err(|e| Error::io(format!("making {}", guest.display()), e))?;
            let exe = std::env::current_exe().map_err(|e| Error::io("finding this binary", e))?;
            let guest_bin = std::env::var_os(GUEST_BIN_ENV)
                .map_or_else(|| exe.with_file_name(GUEST_BIN), PathBuf::from);
            std::fs::copy(&guest_bin, guest.join(GUEST_BIN)).map_err(|e| {
                Error::io(
                    format!(
                        "copying the guest binary from {} (set {GUEST_BIN_ENV} to name another)",
                        guest_bin.display()
                    ),
                    e,
                )
            })?;

            let console = dir.join("console.log");
            let log = std::fs::File::create(&console)
                .map_err(|e| Error::io(format!("making {}", console.display()), e))?;
            let mut process = Command::new(exe)
                .arg(SUBCOMMAND)
                .arg(serde_json::to_string(&config).expect("a machine's config serializes"))
                .arg(&guest)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(log)
                .kill_on_drop(true)
                .spawn()
                .map_err(|e| Error::io("starting the machine's process", e))?;
            // Buffered, because the handshake is read off the same stream the frames are.
            let mut incoming = BufReader::new(process.stdout.take().expect("piped"));
            let outgoing = process.stdin.take().expect("piped");

            // A virtio-console port drops what the host writes until the guest has it open, so
            // nothing is sent before the guest says it has. The three ways it does not, told
            // apart: the process exits, the guest closes the channel, or nothing happens at all.
            // Each with the end of what the console said.
            let failed = |what: String| {
                let said = std::fs::read(&console).unwrap_or_default();
                let said = String::from_utf8_lossy(&said[said.len().saturating_sub(4096)..]);
                Error::DidNotStart(match said.trim() {
                    "" => what,
                    said => format!("{what}; the console said:\n{said}"),
                })
            };
            let mut heard = [0; HANDSHAKE.len()];
            tokio::select! {
                read = incoming.read_exact(&mut heard) => match read {
                    Ok(_) if &heard == HANDSHAKE => {}
                    Ok(_) => {
                        return Err(failed(format!("the guest said {heard:?} for a handshake")));
                    }
                    Err(e) => return Err(failed(format!("the guest did not come up: {e}"))),
                },
                status = process.wait() => return Err(failed(match status {
                    Ok(status) => {
                        format!("the machine exited with {status} before the guest came up")
                    }
                    Err(e) => format!("waiting for the machine: {e}"),
                })),
                () = tokio::time::sleep(BOOT_TIMEOUT) => return Err(failed(format!(
                    "the guest did not come up within {}s",
                    BOOT_TIMEOUT.as_secs()
                ))),
            }
            Ok((process, incoming, outgoing))
        }
        .await;
        let (process, mut incoming, mut stdin) = match started {
            Ok(started) => started,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&dir);
                return Err(e);
            }
        };

        let calls: Calls = Arc::new(Mutex::new(Some(HashMap::new())));
        let reading = Arc::clone(&calls);
        let reader = tokio::spawn(async move {
            while let Ok(Some(message)) = recv::<_, FromGuest>(&mut incoming).await {
                let (id, last) = match &message {
                    FromGuest::Line { id, .. } => (*id, false),
                    FromGuest::Exited { id, .. }
                    | FromGuest::TimedOut { id }
                    | FromGuest::Failed { id, .. }
                    | FromGuest::Synced { id } => (*id, true),
                };
                let mut calls = reading.lock().unwrap();
                let Some(calls) = calls.as_mut() else { break };
                let call = match last {
                    true => calls.remove(&id),
                    false => calls.get(&id).cloned(),
                };
                if let Some(call) = call {
                    let _ = call.send(message);
                }
            }
            *reading.lock().unwrap() = None;
        });
        let (outgoing, mut queued) = mpsc::unbounded_channel();
        let writer = tokio::spawn(async move {
            while let Some(call) = queued.recv().await {
                // One not sent is never answered, and its call hears so with the channel's end.
                if send(&mut stdin, &call).await.is_err() {
                    break;
                }
            }
        });
        Ok(Machine {
            config,
            process: tokio::sync::Mutex::new(process),
            outgoing,
            calls,
            next: AtomicU64::new(1),
            running: AtomicUsize::new(0),
            used: Mutex::new(Instant::now()),
            reader,
            writer,
            dir,
        })
    }

    /// What it was started as.
    pub fn config(&self) -> &MachineConfig {
        &self.config
    }

    /// Whether its process is still up, and not being stopped.
    pub fn running(&self) -> bool {
        self.process
            .try_lock()
            .is_ok_and(|mut process| matches!(process.try_wait(), Ok(None)))
    }

    /// How long since a command last ran in it, or since it started; `None` while one runs.
    pub fn idle(&self) -> Option<Duration> {
        (self.running.load(Ordering::Relaxed) == 0).then(|| self.used.lock().unwrap().elapsed())
    }

    /// Run `cmd` in the guest, handing each line it writes to `line`, and answer its exit
    /// code. Dropped before then, the command is killed, and everything it started.
    pub async fn exec(
        &self,
        cmd: Vec<String>,
        timeout_ms: Option<u64>,
        mut line: impl FnMut(String),
    ) -> Result<i32> {
        let (id, mut answers) = self.call(|id| ToGuest::Exec {
            id,
            cmd,
            timeout_ms,
        })?;
        self.running.fetch_add(1, Ordering::Relaxed);
        let mut pending = Pending {
            machine: self,
            id,
            ended: false,
        };
        let ended = loop {
            match answers.recv().await {
                Some(FromGuest::Line { line: said, .. }) => line(said),
                Some(FromGuest::Exited { code, .. }) => break Ok(code),
                Some(FromGuest::TimedOut { .. }) => {
                    break Err(Error::TimedOut(timeout_ms.unwrap_or_default()));
                }
                Some(FromGuest::Failed { message, .. }) => break Err(Error::NotStarted(message)),
                Some(other) => break Err(Error::Unexpected(format!("{other:?}"))),
                None => break Err(Error::Gone),
            }
        };
        pending.ended = true;
        ended
    }

    /// Take it down, and wait until it is: what it held, it no longer does. Over a snapshot,
    /// the guest puts what it wrote on the disk first, so the next machine over it finds it
    /// there.
    pub async fn stop(&self) -> Result<()> {
        let synced = match self.config.snapshot.is_some() && self.running() {
            true => match self.call(|id| ToGuest::Sync { id })?.1.recv().await {
                Some(FromGuest::Synced { .. }) => Ok(()),
                Some(FromGuest::Failed { message, .. }) => Err(Error::Unexpected(message)),
                Some(other) => Err(Error::Unexpected(format!("{other:?}"))),
                None => Err(Error::Gone),
            },
            false => Ok(()),
        };
        let _ = self.process.lock().await.kill().await;
        synced
    }

    /// Send the call `made` makes of a fresh id, and answer the id and where its answers will
    /// come.
    fn call(
        &self,
        made: impl FnOnce(u64) -> ToGuest,
    ) -> Result<(u64, mpsc::UnboundedReceiver<FromGuest>)> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (sender, answers) = mpsc::unbounded_channel();
        match &mut *self.calls.lock().unwrap() {
            Some(calls) => calls.insert(id, sender),
            None => return Err(Error::Gone),
        };
        self.outgoing.send(made(id)).map_err(|_| Error::Gone)?;
        Ok((id, answers))
    }
}

/// An exec under way: counted as running until it drops, and killed in the guest if it drops
/// before it has ended -- its call given up on.
struct Pending<'a> {
    machine: &'a Machine,
    id: u64,
    ended: bool,
}

impl Drop for Pending<'_> {
    fn drop(&mut self) {
        let machine = self.machine;
        if !self.ended {
            if let Some(calls) = &mut *machine.calls.lock().unwrap() {
                calls.remove(&self.id);
            }
            let _ = machine.outgoing.send(ToGuest::Kill { id: self.id });
        }
        *machine.used.lock().unwrap() = Instant::now();
        machine.running.fetch_sub(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the machine's process is handed is what the parent wrote.
    #[test]
    fn a_config_survives_the_trip_to_the_machine_s_process() {
        let mut config = Config::default();
        config.set_env(Some(vec!["A=1".to_string()]));
        let sent = MachineConfig {
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
        let read: MachineConfig =
            serde_json::from_str(&serde_json::to_string(&sent).unwrap()).unwrap();
        assert_eq!(read.image, sent.image);
        assert_eq!(read.rootfs, sent.rootfs);
        assert_eq!(read.config, sent.config);
        assert_eq!(read.snapshot, sent.snapshot);
        assert_eq!(read.volumes, sent.volumes);
        assert_eq!(read.network, sent.network);
        assert_eq!(read.ports, sent.ports);
    }
}
