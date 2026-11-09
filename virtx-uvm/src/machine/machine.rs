use std::collections::VecDeque;
use std::convert::Infallible;
use std::ffi::OsString;
use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, mpsc};
use std::time::{Duration, Instant};

use oci_spec::image::Config;
use serde::{Deserialize, Serialize};

use crate::{
    machine::{
        Error, Network, Result,
        boot::{self, BootArgs},
        wire::{self, CtrlReq, CtrlResp, IoReq, IoResp},
    },
    snapshot::Snapshot,
};

/// How many of the last lines the machine process logged a failure is told with.
const TAIL: usize = 50;

/// How long a machine told to shut down has to, before it is killed.
pub(super) const STOP_TIMEOUT: Duration = Duration::from_secs(10);

/// How long [`Machine::io`] waits for the guest's answer, from when it is asked, before it
/// gives up on it.
pub(super) const IO_TIMEOUT: Duration = Duration::from_secs(60);

/// What a machine is: the root filesystem it boots, the image's config it runs under, where
/// its writes go, and how big a machine runs them.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MachineConfig {
    /// Its root filesystem's disk, as [`Store::insert_rootfs`](crate::ImageStore::insert_rootfs)
    /// answers it.
    pub rootfs: PathBuf,

    /// The image's runtime config: the environment and working directory the guest, and every
    /// command it runs, is given. Its entrypoint and command are not run.
    pub config: Config,

    /// Where the machine's writes go. `None` is a temporary one, gone with the machine.
    pub snapshot: Option<Snapshot>,

    /// Whether whoever starts the machine holds [`snapshot`](MachineConfig::snapshot)'s lock
    /// for it, for as long as the machine runs and past it; else the machine takes the lock
    /// itself, and lets go of it as it ends.
    #[serde(default)]
    pub snapshot_held: bool,

    /// How big it is, and where it may connect to.
    pub spec: MachineSpec,

    /// The volumes the guest sees, in the order they are mounted.
    #[serde(default)]
    pub volumes: Vec<VolumeMount>,

    /// TCP ports in the guest the machine's process reaches, each on its loopback at a port
    /// of its choosing: the way in, which does not need a
    /// [`network`](MachineSpec::network).
    #[serde(default)]
    pub ports: Vec<u16>,
}

/// How big a machine is and where it may connect to, whatever it boots and runs.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MachineSpec {
    pub vcpus: u8,

    pub ram_mib: u32,

    /// A virtio-gpu device carrying Vulkan, by way of venus. **Ignored unless the crate was
    /// built with the `gpu` feature**: a build that cannot serve Vulkan runs the machine
    /// without it rather than refusing to start. The image has to carry the guest half.
    pub gpu: bool,

    /// How much memory the guest's GPU may hold, in mebibytes. `None` is libkrun's 8 GiB.
    /// Ignored without [`gpu`](MachineSpec::gpu).
    pub gpu_memory_mib: Option<u32>,

    /// Where the guest's own connections may go: the way out.
    #[serde(default)]
    pub network: Network,
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

/// A running machine: its process, and the control channel to it. Dropped, the process is
/// killed, and what the guest had not yet written to its disk is lost.
///
/// Shared between threads: one waits on [`recv`](Machine::recv) while others
/// [`exec`](Machine::exec), [`signal`](Machine::signal), [`kill`](Machine::kill) and ask of its
/// files with [`io`](Machine::io).
pub struct Machine {
    process: Mutex<Child>,
    ctrlin: Mutex<io::PipeWriter>,
    ctrlout: Mutex<io::PipeReader>,
    /// Where requests go to the thread that holds `ioin` and `ioout`, which asks them one at a
    /// time.
    io: mpsc::Sender<IoJob>,
    /// Where on this host's loopback each of its [`ports`](MachineConfig::ports) is reached.
    pub published: Vec<(u16, SocketAddr)>,
}

impl Drop for Machine {
    fn drop(&mut self) {
        let process = self.process.get_mut().unwrap();
        let _ = process.kill();
        let _ = process.wait();
    }
}

impl Machine {
    /// Boot `config` in a process of its own, and answer once the guest takes commands.
    ///
    /// The process is `cmd` -- a program and its arguments -- with the machine's boot args
    /// after them as its last argument. The program has to call [`enter`] first thing in its
    /// `main` when it is run so, and on macOS be signed with the
    /// `com.apple.security.hypervisor` entitlement.
    pub fn start(config: MachineConfig, cmd: &[OsString]) -> Result<Machine> {
        let Some((program, cmd)) = cmd.split_first() else {
            return Err(Error::NoCommand);
        };
        let (their_ctrlin, ctrlin) = io::pipe().map_err(|e| Error::io("making ctrlin", e))?;
        let (mut ctrlout, their_ctrlout) =
            io::pipe().map_err(|e| Error::io("making ctrlout", e))?;
        let (their_ioin, ioin) = io::pipe().map_err(|e| Error::io("making ioin", e))?;
        let (ioout, their_ioout) = io::pipe().map_err(|e| Error::io("making ioout", e))?;
        let args = BootArgs {
            config,
            ctrlin: raw(&their_ctrlin),
            ctrlout: raw(&their_ctrlout),
            ioin: raw(&their_ioin),
            ioout: raw(&their_ioout),
        };
        let mut command = Command::new(program);
        command
            .args(cmd)
            .arg(serde_json::to_string(&args).expect("a machine's boot args serialize"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        inherit(
            &mut command,
            [args.ctrlin, args.ctrlout, args.ioin, args.ioout],
        )
        .map_err(|e| Error::io("handing the machine its channels", e))?;
        let mut process = command
            .spawn()
            .map_err(|e| Error::io("starting the machine's process", e))?;
        // The process's now: held here too, they would keep the channels open past its end.
        drop((their_ctrlin, their_ctrlout, their_ioin, their_ioout));

        let mut tail = VecDeque::with_capacity(TAIL);
        loop {
            match wire::recv(&mut ctrlout).ok().flatten() {
                Some(CtrlResp::Ready(published)) => {
                    return Ok(Machine {
                        process: Mutex::new(process),
                        ctrlin: Mutex::new(ctrlin),
                        ctrlout: Mutex::new(ctrlout),
                        io: files(ioin, ioout),
                        published,
                    });
                }
                Some(CtrlResp::Log { message, .. }) => {
                    if tail.len() == TAIL {
                        tail.pop_front();
                    }
                    tail.push_back(message);
                }
                Some(_) => {}
                None => {
                    let status = process
                        .wait()
                        .map_err(|e| Error::io("waiting for the machine's process", e))?;
                    let why = format!("the machine's process ended with {status} before it was up");
                    return Err(Error::DidNotStart(match tail.is_empty() {
                        true => why,
                        false => format!("{why}; last logged:\n{}", Vec::from(tail).join("\n")),
                    }));
                }
            }
        }
    }

    /// Start `cmd` in the guest, beside whatever else runs there. Answered by a
    /// [`Started`](CtrlResp::Started) from [`recv`](Machine::recv), each `exec` in turn,
    /// with its pid there: the id its output and its end come back under.
    pub fn exec(&self, cmd: Vec<String>) -> Result<()> {
        if cmd.is_empty() {
            return Err(Error::NoCommand);
        }
        wire::send(&mut *self.ctrlin.lock().unwrap(), &CtrlReq::Exec { cmd })
            .map_err(|e| Error::io("sending the command", e))
    }

    /// Send `signal`, as Linux numbers it, to the process `id` and everything it started.
    pub fn signal(&self, id: u32, signal: i32) -> Result<()> {
        wire::send(
            &mut *self.ctrlin.lock().unwrap(),
            &CtrlReq::Signal { id, signal },
        )
        .map_err(|e| Error::io("sending the signal", e))
    }

    /// Have the guest put what it has written on its disks and end, killing it if it has not
    /// by [`STOP_TIMEOUT`], and answer once its process has ended and let go of what it held,
    /// so [`recv`](Machine::recv) ends.
    pub fn stop(&self) {
        let _ = wire::send(&mut *self.ctrlin.lock().unwrap(), &CtrlReq::Shutdown);
        let mut process = self.process.lock().unwrap();
        let until = Instant::now() + STOP_TIMEOUT;
        while matches!(process.try_wait(), Ok(None)) && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = process.kill();
        let _ = process.wait();
    }

    /// Kill the machine's process, and answer once it has ended and let go of what it held, so
    /// [`recv`](Machine::recv) ends. What the guest had not yet written to its disks is lost.
    pub fn kill(&self) {
        let mut process = self.process.lock().unwrap();
        let _ = process.kill();
        let _ = process.wait();
    }

    /// Ask `request` of the guest's files, with `data` for a [`Write`](IoReq::Write),
    /// and wait for the answer, with what it read for a [`Read`](IoReq::Read). One
    /// request at a time: another waits until this one is answered, and neither waits past
    /// [`IO_TIMEOUT`]. One given up on is not asked, unless it already was.
    pub fn io(&self, request: &IoReq, data: &[u8]) -> Result<(IoResp, Vec<u8>)> {
        let deadline = Instant::now() + IO_TIMEOUT;
        let (answer, answered) = mpsc::sync_channel(1);
        let job = IoJob {
            request: request.clone(),
            data: data.to_vec(),
            deadline,
            answer,
        };
        let gone = || Error::io("asking the guest's files", io::ErrorKind::BrokenPipe.into());
        self.io.send(job).map_err(|_| gone())?;
        match answered.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(answer) => answer,
            Err(mpsc::RecvTimeoutError::Timeout) => Err(Error::io(
                "waiting for the guest's files",
                io::ErrorKind::TimedOut.into(),
            )),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(gone()),
        }
    }

    /// The next thing the machine says, waiting for it; `None` once its process has ended.
    /// One thread at a time waits here.
    pub fn recv(&self) -> Option<CtrlResp> {
        wire::recv(&mut *self.ctrlout.lock().unwrap())
            .ok()
            .flatten()
    }
}

/// A request for the thread [`files`] starts, and where its answer goes.
struct IoJob {
    request: IoReq,
    data: Vec<u8>,
    /// When whoever asked stops waiting.
    deadline: Instant,
    answer: mpsc::SyncSender<Result<(IoResp, Vec<u8>)>>,
}

/// Start the thread that asks the guest's files over `ioin` and `ioout`, a request at a time,
/// and answer where requests for it go. It ends with the [`Machine`], or once the channel has.
fn files(mut ioin: io::PipeWriter, mut ioout: io::PipeReader) -> mpsc::Sender<IoJob> {
    let (jobs, queued) = mpsc::channel::<IoJob>();
    std::thread::spawn(move || {
        for job in queued {
            if Instant::now() >= job.deadline {
                continue;
            }
            let answer = exchange(&mut ioin, &mut ioout, &job.request, &job.data);
            let broken = answer.is_err();
            let _ = job.answer.send(answer);
            if broken {
                // A frame cut short leaves the channel out of step for good.
                break;
            }
        }
    });
    jobs
}

/// Send `request`, with `data` for a [`Write`](IoReq::Write), and read its answer.
fn exchange(
    ioin: &mut io::PipeWriter,
    ioout: &mut io::PipeReader,
    request: &IoReq,
    data: &[u8],
) -> Result<(IoResp, Vec<u8>)> {
    wire::send(ioin, request).map_err(|e| Error::io("sending to the guest's files", e))?;
    if matches!(request, IoReq::Write { .. }) {
        wire::send_data(ioin, data).map_err(|e| Error::io("sending to the guest's files", e))?;
    }
    let answer = wire::recv::<IoResp>(ioout)
        .and_then(|answer| answer.ok_or_else(|| io::ErrorKind::UnexpectedEof.into()))
        .map_err(|e| Error::io("reading the guest's answer", e))?;
    let read = match answer {
        IoResp::Data => {
            wire::recv_data(ioout).map_err(|e| Error::io("reading the guest's answer", e))?
        }
        _ => Vec::new(),
    };
    Ok((answer, read))
}

/// A pipe end as the command line names it.
///
/// Neither std nor tokio hands a child anything past its stdin, stdout and stderr, so the
/// channels' ends are kept open across `exec` and named on its command line: by descriptor
/// number on Unix, by handle value on Windows.
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
fn inherit(command: &mut Command, ends: [u64; 4]) -> io::Result<()> {
    use std::os::unix::process::CommandExt as _;

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
fn inherit(_: &mut Command, ends: [u64; 4]) -> io::Result<()> {
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

/// Become the machine this process was spawned with by [`Machine::start`]. Does not come back
/// unless the machine could not be built or broke, and either way tells the host why first.
pub fn enter() -> Result<Infallible> {
    let args = std::env::args_os().last().unwrap_or_default();
    let args: BootArgs = serde_json::from_slice(args.as_encoded_bytes())
        .map_err(|e| Error::io("reading the machine to be", io::Error::other(e)))?;
    // SAFETY: `start` left these open for this process, and named them to it alone.
    let ctrlin = unsafe { boot::hold(args.ctrlin, args.ctrlout) };
    // SAFETY: as above.
    let io = unsafe { (boot::from_raw(args.ioin), boot::from_raw(args.ioout)) };
    let entered = boot::boot(args.config, ctrlin, io);
    if let Err(e) = &entered {
        log::error!("{e}");
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
        let sent = MachineConfig {
            rootfs: PathBuf::from("/store/rootfs/sha256/abc.vmdk"),
            config,
            snapshot: Some(Snapshot {
                name: "work".to_string(),
                path: PathBuf::from("/snapshot/work.ext4"),
            }),
            snapshot_held: true,
            spec: MachineSpec {
                vcpus: 2,
                ram_mib: 2048,
                gpu: false,
                gpu_memory_mib: None,
                network: Network::Public,
            },
            volumes: vec![VolumeMount {
                path: PathBuf::from("/store/volume/data"),
                at: "/data".to_string(),
                read_only: true,
            }],
            ports: vec![3000],
        };
        let read: MachineConfig =
            serde_json::from_str(&serde_json::to_string(&sent).unwrap()).unwrap();
        assert_eq!(read.rootfs, sent.rootfs);
        assert_eq!(read.config, sent.config);
        assert_eq!(read.snapshot, sent.snapshot);
        assert_eq!(read.volumes, sent.volumes);
        assert_eq!(read.spec.network, sent.spec.network);
        assert_eq!(read.ports, sent.ports);
    }
}
