//! Booting a [`Machine`](crate::machine::Machine) from the outside: a process that becomes
//! one, spawned over what it needs on disk, and talked to down a socket that boot turned into
//! a console port.
//!
//! **Nothing here holds a hypervisor.** A machine is entered by the process that becomes it
//! and that process does not come back (see [`Machine::enter`](crate::machine::Machine::enter)),
//! so whoever wants to *use* one -- a session, a build -- spawns it and holds a [`Boot`].
//! Dropping the [`Boot`] kills that process, which is the only way a machine is stopped.
//!
//! What a boot is -- which program, with which arguments in front of its own, and what the
//! guest sends once it is there -- is the caller's to say to [`Boot::start`]: a library cannot
//! know which binary on this host carries the entitlement, nor which guest binary was built
//! with it.
//!
//! What this end tells a boot is [`told`], and the boot's own `main` is where those arguments
//! are read. No type is shared between the two: an argument either end gets wrong is refused
//! by name before a hypervisor is touched.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use virtx::protocol::{
    Call, Message, Response,
    stdio::{read, write},
};

use crate::machine::{Incoming, Listener, Mount, Outgoing, Upper, host_memory};

/// The shape a session's machine gets when its client did not ask for one.
///
/// This server's own environment: a client that says nothing about the size of its machine
/// is not asking for one size over another, it is leaving the question to whoever started
/// this process -- who is the only party here that knows what the host it runs on can spare.
/// A client that does have an opinion says so, and what it says wins.
///
/// Read here and passed on the command line, rather than left for the boot to find in the
/// environment it inherits: what a machine was given is then something this end can be asked,
/// and the boot has one place it learns its shape from.
pub const VCPUS_ENV: &str = "VIRTX_UVM_VCPUS";
pub const MEMORY_ENV: &str = "VIRTX_UVM_MEMORY_MIB";
pub const GPU_ENV: &str = "VIRTX_UVM_GPU";
pub const GPU_MEMORY_ENV: &str = "VIRTX_UVM_GPU_MEMORY_MIB";

/// How much a session may write, in gibibytes, when its client did not say -- the operator's
/// default for the same reason the machine's shape is. Not part of [`Shape`], because it is
/// not the boot's to know: the disk is made here, once per session, and a boot is only
/// handed the file.
pub const DISK_ENV: &str = "VIRTX_UVM_DISK_GIB";

/// How long to wait for the guest to come up before saying it did not.
const BOOT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Where each published guest port can be reached right now: its console port against the
/// loopback port the boot that is up relays it from, or `None` while no boot is up.
///
/// Watched rather than asked, because the doors a client connects to are the session's and
/// outlive every boot: a connection that arrives between a `stop` and the next boot waits on
/// this for a machine to join it to -- see the session's `door`.
pub type Relays = std::sync::Arc<tokio::sync::watch::Sender<Option<HashMap<u16, u16>>>>;

/// Every way into a machine from this host -- see [`virtx::protocol::Port`].
#[derive(Clone, Default)]
pub struct Doors {
    /// The guest ports something here may connect into. Each is relayed by the boot, and
    /// where the relays are is said in [`relays`](Self::relays) once the guest is up.
    pub inbound: Vec<u16>,
    pub relays: Relays,
}

/// The machine a session runs in: how much of the host it gets, and whether it has a Vulkan
/// device.
///
/// What [`InitCall`](virtx::protocol::InitCall)'s three machine members came to on this side,
/// each already resolved against this server's own defaults -- so what reaches a boot is one
/// answer per question and not a client's wish beside an operator's.
///
/// `None` is not a number this end picks: it is the boot's own default, which is the last
/// place left that has one. A `0` never reaches here -- it is refused where an `init` is read,
/// since a machine nothing runs on is a request that has to change rather than a shape to
/// spell.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Shape {
    pub vcpus: Option<u8>,
    pub memory_mib: Option<u32>,
    pub gpu: bool,
    /// What the GPU may hold, in mebibytes -- only ever `Some` beside a `gpu` of `true`.
    pub gpu_memory_mib: Option<u32>,
}

impl Shape {
    /// What this server makes of a machine nobody described: its operator's overrides, and
    /// the boot's defaults where there are none.
    pub fn from_env() -> Shape {
        Shape {
            // A zero from the environment is dropped rather than refused, the way an
            // unparseable one is: these are a knob for whoever started this process, and what
            // a typo costs is the override.
            vcpus: number::<u8>(VCPUS_ENV).filter(|vcpus| *vcpus > 0),
            memory_mib: number::<u32>(MEMORY_ENV).filter(|mib| *mib > 0),
            gpu: flag(GPU_ENV),
            gpu_memory_mib: number::<u32>(GPU_MEMORY_ENV).filter(|mib| *mib > 0),
        }
    }

    /// What a build gets: half the host's processors and half its memory, and never a GPU.
    ///
    /// A build is the host's own work and nobody else's session, so it is sized to the host
    /// rather than to the default a session gets. Where the host cannot be read, the boot's
    /// default stands in.
    pub fn for_build() -> Shape {
        Shape {
            vcpus: std::thread::available_parallelism()
                .ok()
                .map(|n| (n.get() / 2).clamp(1, usize::from(u8::MAX)) as u8),
            memory_mib: host_memory()
                .map(|bytes| (bytes >> 21).clamp(1, u64::from(u32::MAX)) as u32),
            gpu: false,
            gpu_memory_mib: None,
        }
    }
}

/// The operator's disk size from [`DISK_ENV`], in bytes, and [`Upper::SIZE`] where there is
/// none -- dropped rather than refused when it is zero or unreadable, the way
/// [`Shape::from_env`]'s numbers are.
pub fn disk_from_env() -> u64 {
    number::<u32>(DISK_ENV)
        .filter(|gib| *gib > 0)
        .map_or(Upper::SIZE, |gib| u64::from(gib) << 30)
}

/// Whether this host can run a machine at all, and if not, what to do about it.
///
/// **Linux and Windows**, the hosts where the answer depends on how the machine was set up
/// rather than on this build. On macOS the hypervisor is the signature's, which
/// `cargo xtask build host` gives.
///
/// On Linux, `/dev/kvm` may be missing, and where it is there, Debian and Ubuntu leave it
/// `0660 root:kvm` and grant it by ACL only to a user logged in at the seat -- so a user
/// over ssh, or a service, is refused it. Without this, either is a boot that exits with
/// libkrun's own sentence in a console log nobody reads. Opened rather than stat'd: the
/// mode, a group and an ACL each decide it, and asking the kernel is the one check that
/// weighs all three the way the boot's open will.
///
/// On Windows, the hypervisor is `WinHvPlatform.dll`, which is there only where the Windows
/// Hypervisor Platform feature is turned on. The boot delay-loads it (see
/// the boot's `build.rs`), so without this its first call into the DLL is an
/// exception rather than a sentence. Whether the hypervisor is *running* -- virtualization
/// on in the firmware -- is the boot's to find out, and libkrun says so when it is not.
pub fn hypervisor() -> anyhow::Result<()> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::LibraryLoader::LoadLibraryW;
        let name: Vec<u16> = "WinHvPlatform.dll\0".encode_utf16().collect();
        // SAFETY: a nul-terminated UTF-16 string that outlives the call. Left loaded: the
        // boot is another process, and this one loading it costs nothing.
        if unsafe { LoadLibraryW(name.as_ptr()) }.is_null() {
            anyhow::bail!(
                "this host has no Windows Hypervisor Platform (WinHvPlatform.dll), so it cannot \
                 run a micro-VM: turn it on with `Enable-WindowsOptionalFeature -Online \
                 -FeatureName HypervisorPlatform` in an elevated PowerShell, or under \
                 \"Turn Windows features on or off\", and restart"
            );
        }
    }

    #[cfg(target_os = "linux")]
    match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/kvm")
    {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => anyhow::bail!(
            "this host has no /dev/kvm, so it cannot run a micro-VM: KVM is off (turn on \
             virtualization in the firmware, and load kvm_intel or kvm_amd), or this is a VM \
             without nested virtualization, or a container started without --device /dev/kvm"
        ),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => anyhow::bail!(
            "this user may not open /dev/kvm, so it cannot run a micro-VM: add it to the kvm \
             group with `sudo usermod -aG kvm $USER`, then log in again"
        ),
        Err(e) => anyhow::bail!("opening /dev/kvm: {e}"),
    }
    Ok(())
}

/// A booted micro-VM, held for as long as there is something to ask it.
///
/// Dropping it takes the machine down and removes its directory -- but not the upper layer,
/// which is the caller's and is what a `stop` is careful to leave.
pub struct Boot {
    /// The process holding the hypervisor. Killed when this drops, which is what takes the
    /// machine down.
    boot: std::process::Child,
    /// The console port, split: one call goes out and one answer comes back, so there is
    /// never more than one outstanding and nothing to pair beyond the id.
    incoming: tokio::io::BufReader<Incoming>,
    outgoing: Outgoing,
    /// The next call's id. A number about the wire and nothing else.
    next: u64,
    /// Everything this boot made and nothing else reads: gone when this drops.
    dir: PathBuf,
    /// Where the session's published ports are relayed from, which stops being this boot's
    /// when it drops. `None` for a machine with nothing published.
    relays: Option<Relays>,
}

impl Drop for Boot {
    fn drop(&mut self) {
        // First, so that no connection is joined to a relay that is about to go: one that
        // arrives from here on waits for the next boot.
        if let Some(relays) = &self.relays {
            relays.send_replace(None);
        }
        let _ = self.boot.kill();
        let _ = self.boot.wait();
        // This boot's, and only this boot's: the disk beside it is the session's, and a
        // `stop` is what drops this.
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Boot {
    /// Boot `image` and wait for the guest to say it is there.
    ///
    /// Returns once the guest has opened the port, not merely once the machine is running --
    /// there is nothing a caller can do with the time in between.
    #[allow(clippy::too_many_arguments)]
    pub async fn start(
        // The program that becomes the machine, and what goes in front of the boot's own
        // arguments -- a subcommand, for a program that is more than a boot.
        program: &Path,
        args: &[OsString],
        // The guest binary, copied into the machine's share, and what it is called in there
        // -- the name the guest is told to exec.
        guest: &Path,
        guest_name: &str,
        // What the guest sends, once, as soon as it has the console port open.
        handshake: &[u8],
        // Virtx's own executables, shared into the guest -- `None` for a host that has none,
        // which is a machine with no `/abin` rather than one that fails.
        abin: Option<&Path>,
        // Everything this boot makes and nothing else reads: made here, and removed when the
        // `Boot` drops. One per machine, since two boots in one would share a console log
        // and a channel.
        dir: &Path,
        image: &str,
        // The writable disk the overlay's upper layer goes on, which is what parts a
        // session's machine from a build's: a session's is kept and carried on, a
        // build's is read once and thrown away.
        upper: &Path,
        mounts: &[Mount],
        network: bool,
        // The ways in, which need `network` -- a port is carried in over it.
        doors: &Doors,
        // How much of the host this machine gets, and whether it has a Vulkan device --
        // settled against the caller's own defaults before it arrived here.
        shape: Shape,
    ) -> anyhow::Result<Boot> {
        use tokio::io::AsyncReadExt as _;

        // Before anything is made for it: a host that cannot run a machine is a sentence
        // about the host, not a boot that exits into a console log.
        hypervisor()?;

        let dir = dir.to_path_buf();
        let guest_dir = dir.join("guest");
        std::fs::create_dir_all(&guest_dir)?;

        let console = dir.join("console.log");

        // A copy and not a link: this directory is served to the guest over virtio-fs, and
        // what it holds has to be a file under it rather than a name pointing out of it.
        let bin = guest_dir.join(guest_name);
        std::fs::copy(guest, &bin)
            .map_err(|e| anyhow::anyhow!("copying {} into the share: {e}", guest.display()))?;
        executable(&bin)
            .map_err(|e| anyhow::anyhow!("making {} executable: {e}", bin.display()))?;

        // Shared as it stands rather than copied, so on Windows its programs are marked
        // executable where they are -- see `abin_executable`.
        #[cfg(windows)]
        if let Some(abin) = abin {
            abin_executable(abin);
        }

        // Bound before the child is started, because the child connects to it as its first
        // act and there is nothing to retry against if it is not there yet. What a boot has
        // to be told to reach it comes back with it: on one host that is a socket's path and
        // on the other a pipe name, and neither is spelled here.
        let (mut listener, channel) = Listener::bind(&dir)
            .map_err(|e| anyhow::anyhow!("binding the console channel: {e}"))?;

        let mut told = told(
            image,
            upper,
            &guest_dir,
            abin,
            &channel,
            network,
            doors,
            &dir.join("relays"),
            shape,
        );
        // In the order the client named them, which is the order the protocol says they are
        // realized in -- so a boot is handed the list as it stands and nothing here sorts or
        // merges it.
        for mount in mounts {
            let mut at = mount.at.clone().into_os_string();
            if mount.read_only {
                at.push(":ro");
            }
            told.extend(["--mount".into(), mount.host.clone().into_os_string(), at]);
        }

        // The guest's console -- kernel messages, a panic, whatever a boot that failed has to
        // say -- to a file rather than to this process's stderr, so that a machine which
        // never connected can still be asked what happened. It is the boot's own stdio
        // because that is where its `Machine` puts the console; nothing in the boot opens a
        // log of its own.
        let log = std::fs::File::create(&console)
            .map_err(|e| anyhow::anyhow!("making {}: {e}", console.display()))?;
        let mut boot = std::process::Command::new(program)
            .args(args)
            .args(&told)
            .stdin(std::process::Stdio::null())
            .stdout(
                log.try_clone()
                    .map_err(|e| anyhow::anyhow!("sharing the console log: {e}"))?,
            )
            .stderr(log)
            .spawn()
            .map_err(|e| anyhow::anyhow!("starting {}: {e}", program.display()))?;

        // Before anything else is done with it: a boot that is not in the job from the moment
        // it exists is one that outlives a server killed in the window before it was.
        #[cfg(windows)]
        reap_with_this_process(&boot);

        // A virtio-console port discards what the host writes while no process in the guest
        // has it open, rather than queueing it -- the socket is up long before the kernel is.
        // So this waits for the guest to say that something is reading, and only then is
        // there a session.
        //
        // Waited for with a deadline and with an eye on the child, because the three ways
        // this does not happen are worth telling apart: a boot that could not start says so
        // by exiting, a guest that died inside says so the same way, and a machine that is
        // merely slow says nothing at all. Without this they are one hang.
        let connected = listener.accept();
        tokio::pin!(connected);
        let until = tokio::time::Instant::now() + BOOT_TIMEOUT;
        let (incoming, outgoing) = loop {
            tokio::select! {
                // The connection first, so that one arriving in the same moment the deadline
                // does is taken rather than reported as a machine that never came.
                biased;
                connected = &mut connected => break connected
                    .map_err(|e| anyhow::anyhow!("waiting for the guest to connect: {e}"))?,
                _ = tokio::time::sleep(std::time::Duration::from_millis(20)) => {
                    if let Some(status) = boot.try_wait()? {
                        anyhow::bail!(
                            "the boot exited with {status} before the guest connected{}",
                            said(&console)
                        );
                    }
                    anyhow::ensure!(
                        tokio::time::Instant::now() < until,
                        "the guest did not connect within {}s{}",
                        BOOT_TIMEOUT.as_secs(),
                        said(&console)
                    );
                }
            }
        };

        // Buffered here rather than where the struct is made, because the handshake is read
        // off the same stream the frames are and a read of eight bytes must not take a
        // buffer's worth of the first frame with it.
        let mut incoming = tokio::io::BufReader::new(incoming);
        let mut heard = vec![0u8; handshake.len()];
        tokio::time::timeout(BOOT_TIMEOUT, incoming.read_exact(&mut heard))
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "the guest connected but did not say it was there within {}s{}",
                    BOOT_TIMEOUT.as_secs(),
                    said(&console)
                )
            })?
            .map_err(|e| {
                anyhow::anyhow!(
                    "waiting for the guest to say it is there: {e}{}",
                    said(&console)
                )
            })?;
        anyhow::ensure!(
            heard == handshake,
            "the guest said {heard:?} where a handshake was expected"
        );

        // Where this boot relays each published port from. Written before the machine
        // started, so a guest that has said it is up is a file that is there -- see the
        // boot's `relay`.
        if !doors.inbound.is_empty() {
            let at = dir.join("relays");
            let said = std::fs::read_to_string(&at)
                .map_err(|e| anyhow::anyhow!("reading where the boot relays from: {e}"))?;
            let relays = said
                .lines()
                .map(|line| {
                    let (console, port) = line
                        .split_once(' ')
                        .ok_or_else(|| anyhow::anyhow!("{line:?} is not a relay"))?;
                    Ok((console.parse::<u16>()?, port.parse::<u16>()?))
                })
                .collect::<anyhow::Result<HashMap<_, _>>>()?;
            doors.relays.send_replace(Some(relays));
        }

        Ok(Boot {
            boot,
            incoming,
            outgoing,
            next: 0,
            dir,
            relays: (!doors.inbound.is_empty()).then(|| doors.relays.clone()),
        })
    }

    pub fn console(&self) -> PathBuf {
        self.dir.join("console.log")
    }

    /// One call out and its answer back.
    ///
    /// There is one outstanding at a time, so an answer carrying a different id is a peer
    /// that has lost its place and a session cannot carry on from one.
    pub async fn call(&mut self, call: Call) -> anyhow::Result<Response> {
        self.next += 1;
        let id = self.next;
        write(&mut self.outgoing, &Message::Request { id, call }).await?;

        match read(&mut self.incoming).await? {
            Some(Message::Response {
                id: answered,
                result,
            }) if answered == id => Ok(result),
            Some(Message::Response { id: answered, .. }) => anyhow::bail!(
                "the guest answered request {answered}, which is not the {id} it was asked"
            ),
            Some(other) => anyhow::bail!("the guest sent {other:?} instead of an answer"),
            None => anyhow::bail!("the guest closed the channel"),
        }
    }
}

/// The boot's command line, less the trees the caller appends -- see its module doc for what
/// each one is.
///
/// Written here and read there, with no type in between. What a shared one would buy is a
/// compile error in place of a refusal, and the refusal is not a quiet one: a boot given an
/// argument it does not know says so and exits before a hypervisor is touched.
///
/// Nothing is decided here. The reach and the shape both arrive already settled -- a client's
/// word where it had one and the operator's environment where it did not -- so this spells what
/// it was handed and a machine's size is answered in one place rather than two.
#[allow(clippy::too_many_arguments)]
fn told(
    image: &str,
    upper: &Path,
    guest: &Path,
    // The caller's answer, so that what a boot is told is a function of its arguments.
    abin: Option<&Path>,
    // What `Listener::bind` answered with, passed on as it stands: a socket's path on one
    // host and a pipe name on the other, and nothing here reads it as either.
    channel: &OsStr,
    network: bool,
    doors: &Doors,
    // Where the boot says where it relays each published port from.
    relays: &Path,
    shape: Shape,
) -> Vec<OsString> {
    let mut told: Vec<OsString> = vec![
        "--image".into(),
        image.into(),
        "--session".into(),
        upper.into(),
        "--guest".into(),
        guest.into(),
        "--channel".into(),
        channel.into(),
    ];
    // A flag, like `--gpu`: a boot without it has no network, which is also what a session
    // that did not ask for one gets.
    if network {
        told.push("--network".into());
    }
    // Left off entirely when this host has none, which is what the boot reads as a machine
    // that gets no `/abin` rather than one with an empty directory at that path.
    if let Some(abin) = abin {
        told.extend(["--abin".into(), abin.into()]);
    }
    for console in &doors.inbound {
        told.extend(["--publish".into(), OsString::from(console.to_string())]);
    }
    if !doors.inbound.is_empty() {
        told.extend(["--relays".into(), relays.into()]);
    }
    if let Some(vcpus) = shape.vcpus {
        told.extend(["--vcpus".into(), OsString::from(vcpus.to_string())]);
    }
    if let Some(memory) = shape.memory_mib {
        told.extend(["--memory-mib".into(), OsString::from(memory.to_string())]);
    }
    if shape.gpu {
        told.push("--gpu".into());
        if let Some(memory) = shape.gpu_memory_mib {
            told.extend([
                "--gpu-memory-mib".into(),
                OsString::from(memory.to_string()),
            ]);
        }
    }
    told
}

/// Mark each program in `abin/` executable, as the guest binary is -- see [`executable`].
///
/// **Windows only.** `abin/` is shared as it stands rather than copied into the machine's
/// directory, and there libkrun's virtiofs answers `0644` for a file with no stream beside
/// it, so without this every `/abin` program is one the guest cannot exec. Nothing else on
/// Windows keeps a mode for it to have: a release unpacked onto NTFS, or built there, has
/// none. Elsewhere the file's own mode is what the guest sees, and it came with the file.
///
/// Every boot, rather than once where the directory is made: it is cheap, and a directory
/// replaced since -- an install, a new release -- is marked again. Only the programs, not
/// `skills/`, which is read and never run. A file it fails on is said and skipped, for the
/// same reason a host with no `abin/` is a session without it.
#[cfg(windows)]
fn abin_executable(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for path in entries.flatten().map(|entry| entry.path()) {
        if !path.is_file() {
            continue;
        }
        if let Err(e) = executable(&path) {
            eprintln!(
                "{} will not be executable in the guest: {e}",
                path.display()
            );
        }
    }
}

/// A default read out of this process's environment, ignoring anything that is not a number.
///
/// A value nobody can act on is not a reason to refuse a session: these are a knob for
/// whoever started the process, and the boot's own default is a machine that works. What a
/// typo costs is the override, which is the smallest thing it can cost.
fn number<T: std::str::FromStr>(key: &str) -> Option<T> {
    std::env::var(key).ok()?.parse().ok()
}

/// A switch read the same way: set to anything but `0` or nothing at all.
fn flag(key: &str) -> bool {
    std::env::var_os(key).is_some_and(|on| !on.is_empty() && on != "0")
}

/// Mark a file the guest is about to exec as executable, as the host's virtiofs server reads
/// executability.
///
/// A mode bit where the host has them. Windows has not, and libkrun's virtiofs there answers
/// `0644` for every regular file unless an alternate data stream beside it says otherwise --
/// so a guest binary copied in with `fs::copy` is one the guest cannot run, and the failure
/// is an `exec` that never happened rather than anything this end would hear about.
///
/// The stream is libkrun's own convention, `uid:gid:0mode` with the mode including its file
/// type, and it is written here rather than asked of the device because the device is not up
/// yet: this is the file being put into the share the machine has not been given.
fn executable(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
    }

    #[cfg(windows)]
    {
        const OVERRIDE_STAT: &str = ":user.containers.override_stat";
        const ROOT_EXECUTABLE: &str = "0:0:0100755";

        let mut stream = path.as_os_str().to_os_string();
        stream.push(OVERRIDE_STAT);
        std::fs::write(PathBuf::from(stream), ROOT_EXECUTABLE)
    }
}

/// The tail of the guest's console, for a failure that happened before there was a channel to
/// say anything on.
fn said(console: &Path) -> String {
    match std::fs::read_to_string(console) {
        Ok(text) if !text.trim().is_empty() => {
            let tail: Vec<&str> = text.lines().rev().take(20).collect();
            format!(
                ". The machine said:\n{}",
                tail.into_iter().rev().collect::<Vec<_>>().join("\n")
            )
        }
        _ => String::new(),
    }
}

/// Tie a boot's lifetime to this process's, so that a server that dies takes its machines
/// with it.
///
/// [`Boot`]'s `Drop` kills the boot, and that covers the ways a server ends by *running* —
/// a session stopped, an error returned, a client that went away. It covers none of the ways
/// a server ends by being ended: a `TerminateProcess`, a panic that aborts, a debugger
/// detaching, the console window closed. There is no unwinding on those paths and so no
/// `Drop`, and what is left is a micro-VM with no one to talk to. Measured here: a run that
/// failed left the boot up for twenty minutes, holding the RAM it had been given,
/// holding the pipe its parent's output was being read through, and holding its own
/// executable open — which is how the next `cargo build` came to fail on a linker that could
/// not replace the file.
///
/// A job object is the one mechanism that does not need this process to be alive to act. The
/// handle is created once and never closed while the server runs; every boot is assigned to
/// it, and `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` tells the kernel to terminate what is in the
/// job when the last handle to it goes — which happens on process exit however that exit came
/// about.
///
/// Best effort, and deliberately so. A machine that booted is worth more than the guarantee
/// that it will be reaped: if the job cannot be made or the child cannot be assigned to it,
/// this says so once and the boot goes on without it, back to being `Drop`'s to kill.
#[cfg(windows)]
fn reap_with_this_process(child: &std::process::Child) {
    use std::os::windows::io::AsRawHandle as _;
    use std::sync::OnceLock;
    use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };

    /// `isize` and not `HANDLE`, because a `OnceLock` holds one value for every thread and a
    /// raw pointer is not `Send`. What it stands for is a handle this process holds until it
    /// exits, which is the whole of what makes the job work.
    static JOB: OnceLock<Option<isize>> = OnceLock::new();

    let job = *JOB.get_or_init(|| unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() || job == INVALID_HANDLE_VALUE {
            return None;
        }
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let set = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &limits as *const _ as *const std::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        // The handle is kept open on purpose -- closing it is what kills the job.
        (set != 0).then_some(job as isize)
    });

    let Some(job) = job else {
        eprintln!("no job object; a boot will outlive a server that is killed rather than stopped");
        return;
    };

    // SAFETY: the child is alive and owns its handle for as long as this borrow lasts.
    let assigned =
        unsafe { AssignProcessToJobObject(job as HANDLE, child.as_raw_handle() as HANDLE) };
    if assigned == 0 {
        eprintln!("this boot is not in the job object and will outlive a server that is killed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `/abin` is spelled when this host has one and left off entirely when it does not --
    /// which is what the boot reads as a machine that gets none, rather than one with an
    /// empty directory at that path.
    #[test]
    fn a_boot_is_told_about_abin_only_when_there_is_one() {
        let line = |abin| {
            told(
                "alpine:3.21",
                Path::new("/s/session.ext4"),
                Path::new("/s/guest"),
                abin,
                OsStr::new("/s/port.sock"),
                false,
                &Doors::default(),
                Path::new("/s/relays"),
                Shape::default(),
            )
            .into_iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
        };

        assert!(!line(None).iter().any(|arg| arg == "--abin"));
        // And a session that asked for no network is a boot told nothing about one.
        assert!(!line(None).iter().any(|arg| arg == "--network"));

        let given = line(Some(Path::new("/h/abin")));
        let at = given.iter().position(|arg| arg == "--abin").unwrap();
        assert_eq!(given[at + 1], "/h/abin");
    }

    /// A network and its published ports are spelled, and where the relays go only when there
    /// is one to relay -- the boot refuses the one without the other.
    #[test]
    fn a_boot_is_told_its_doors() {
        let line = |doors: &Doors| {
            told(
                "alpine:3.21",
                Path::new("/s/session.ext4"),
                Path::new("/s/guest"),
                None,
                OsStr::new("/s/port.sock"),
                true,
                doors,
                Path::new("/s/relays"),
                Shape::default(),
            )
            .into_iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
        };

        let closed = line(&Doors::default());
        assert!(closed.iter().any(|arg| arg == "--network"));
        assert!(!closed.iter().any(|arg| arg == "--relays"));

        let inward = line(&Doors {
            inbound: vec![5900],
            ..Doors::default()
        });
        let at = inward.iter().position(|arg| arg == "--publish").unwrap();
        assert_eq!(inward[at + 1], "5900");
        let at = inward.iter().position(|arg| arg == "--relays").unwrap();
        assert_eq!(inward[at + 1], "/s/relays");
    }

    /// A shape is spelled only where it was answered, and the boot's own defaults are what an
    /// unspelled one leaves in place -- so a machine nobody described is a command line with
    /// nothing about one on it.
    #[test]
    fn a_boot_is_told_the_shape_it_was_given() {
        let line = |shape| {
            told(
                "alpine:3.21",
                Path::new("/s/session.ext4"),
                Path::new("/s/guest"),
                None,
                OsStr::new("/s/port.sock"),
                false,
                &Doors::default(),
                Path::new("/s/relays"),
                shape,
            )
            .into_iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
        };

        let quiet = line(Shape::default());
        assert!(!quiet.iter().any(|arg| arg == "--vcpus"));
        assert!(!quiet.iter().any(|arg| arg == "--memory-mib"));
        assert!(!quiet.iter().any(|arg| arg == "--gpu"));
        assert!(!quiet.iter().any(|arg| arg == "--gpu-memory-mib"));

        let asked = line(Shape {
            vcpus: Some(4),
            memory_mib: Some(4096),
            gpu: true,
            gpu_memory_mib: Some(8192),
        });
        let after = |flag: &str| {
            asked
                .iter()
                .position(|arg| arg == flag)
                .map(|at| asked[at + 1].clone())
        };
        assert_eq!(after("--vcpus").as_deref(), Some("4"));
        assert_eq!(after("--memory-mib").as_deref(), Some("4096"));
        assert!(asked.iter().any(|arg| arg == "--gpu"));
        assert_eq!(after("--gpu-memory-mib").as_deref(), Some("8192"));
    }
}
