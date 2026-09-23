//! A micro-VM, booted on an image this server made sure was in the cache.
//!
//! **Nothing here holds a hypervisor.** What this module does is make what a machine needs on
//! disk, spawn `cortex-krun-host` over it, and talk to the guest down a socket that boot
//! turned into a console port. The VMM and the entitlement it needs are both in that other
//! binary -- see [`halves`](super::halves) for where it and the guest binary are found.
//!
//! What this end tells a boot is [`told`], and the boot's own module doc is where those
//! arguments are written down. No type is shared between the two: they are one repository
//! built by one `cargo build`, and an argument either end gets wrong is refused by name
//! before a hypervisor is touched.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use cortex::console::{
    Call, Message, Response,
    stdio::{read, write},
};
use cortex_krun::session::Network;
use cortex_krun_host::{CAPABILITIES_ARG, GPU_CAPABILITY, GUEST_BIN};

/// The reach a session gets when its client did not ask for one, and the host ports granted
/// with it.
///
/// Read from this server's own environment, because a client that says nothing about a
/// network is not asking for one setting over another -- it is leaving the question to
/// whoever started this process, who is the only party here that knows what the machine it
/// runs on is allowed to talk to. A client that does have an opinion says so, and what it
/// says wins.
pub const NETWORK_ENV: &str = "CORTEX_KRUN_NETWORK";
pub const HOST_PORTS_ENV: &str = "CORTEX_KRUN_HOST_PORTS";

/// The shape a session's machine gets when its client did not ask for one.
///
/// This server's own environment, read for the reason [`NETWORK_ENV`] is: a client that says
/// nothing about the size of its machine is not asking for one size over another, it is
/// leaving the question to whoever started this process -- who is the only party here that
/// knows what the host it runs on can spare. A client that does have an opinion says so, and
/// what it says wins.
///
/// Read here and passed on the command line, rather than left for the boot to find in the
/// environment it inherits: what a machine was given is then something this end can be asked,
/// and the boot has one place it learns its shape from.
pub const VCPUS_ENV: &str = "CORTEX_KRUN_VCPUS";
pub const MEMORY_ENV: &str = "CORTEX_KRUN_MEMORY_MIB";
pub const GPU_ENV: &str = "CORTEX_KRUN_GPU";

/// How long to wait for the guest to come up before saying it did not.
const BOOT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// A host directory the guest can see, where it sees it, and whether a command may write
/// in it.
///
/// What a [`MountSpec`](cortex::console::MountSpec) came to on this side: the URL already
/// read as a directory, and the guest path and the `ro` carried on as they arrived.
#[derive(Clone)]
pub struct Mount {
    pub at: String,
    pub from: PathBuf,
    pub read_only: bool,
}

/// The machine a session runs in: how much of the host it gets, and whether it has a Vulkan
/// device.
///
/// What [`InitCall`](cortex::console::InitCall)'s three machine members came to on this side,
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
        }
    }
}

/// Whether the boot beside this binary can attach a GPU, asked of that boot itself.
///
/// **Asked rather than assumed**, because it is a fact about *that file*: the two binaries are
/// built by one `cargo build` but a cargo feature is not, and `CORTEX_KRUN_BOOT_BIN` can name
/// a boot from another build entirely. Assuming would mean one of the two failures this
/// protocol has no room for -- a session refused for a device the boot had, or a session
/// taken, booted and quietly run on the CPU.
///
/// Once per process and only when something wants a GPU. The answer cannot change under a
/// running server -- it is a file's, and the file is resolved the same way every boot resolves
/// it -- so the spawn is paid by the first session that asks and by none after it. A boot that
/// could not be asked is an error and not a `false`: this end knows nothing either way, and
/// what it must not do is decide.
pub fn can_gpu() -> anyhow::Result<bool> {
    // The failure is kept as a sentence rather than as the error it was, because a `OnceLock`
    // holds one value for every caller and `anyhow::Error` is nobody's to clone.
    static ASKED: std::sync::OnceLock<Result<Vec<String>, String>> = std::sync::OnceLock::new();

    match ASKED.get_or_init(|| capabilities().map_err(|e| format!("{e:#}"))) {
        Ok(names) => Ok(names.iter().any(|name| name == GPU_CAPABILITY)),
        Err(said) => Err(anyhow::anyhow!("{said}")),
    }
}

/// Everything the boot says it can make, one name to a line.
fn capabilities() -> anyhow::Result<Vec<String>> {
    let program = super::halves::boot()?;
    let said = std::process::Command::new(&program)
        .arg(CAPABILITIES_ARG)
        .output()
        .map_err(|e| anyhow::anyhow!("asking {} what it can make: {e}", program.display()))?;
    anyhow::ensure!(
        said.status.success(),
        "{} answered {CAPABILITIES_ARG} with {}: {}",
        program.display(),
        said.status,
        String::from_utf8_lossy(&said.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&said.stdout)
        .lines()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect())
}

/// A booted micro-VM, held for as long as there is something to ask it.
///
/// Dropping it takes the machine down and removes everything the boot made -- but not the
/// session's disk, which is beside it and is what a `stop` is careful to leave.
pub struct Machine {
    /// The signed copy holding the hypervisor. Killed when this drops, which is what takes
    /// the machine down.
    boot: std::process::Child,
    /// The console port, split: one call goes out and one answer comes back, so there is
    /// never more than one outstanding and nothing to pair beyond the id.
    incoming: tokio::io::BufReader<tokio::net::unix::OwnedReadHalf>,
    outgoing: tokio::net::unix::OwnedWriteHalf,
    /// The next call's id. A number about the wire and nothing else.
    next: u64,
    /// Everything this boot made and nothing else reads: gone when this drops.
    dir: PathBuf,
}

impl Drop for Machine {
    fn drop(&mut self) {
        let _ = self.boot.kill();
        let _ = self.boot.wait();
        // This boot's, and only this boot's: the disk beside it is the session's, and a
        // `stop` is what drops this.
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Machine {
    /// Boot `image` and wait for the guest to say it is there.
    ///
    /// Returns once the guest has opened the port, not merely once the machine is running --
    /// there is nothing a caller can do with the time in between.
    pub async fn boot(
        image: &str,
        // The writable disk the overlay's upper layer goes on, which is what parts a
        // session's machine from a build's: a session's is kept and carried on, a
        // build's is read once and thrown away.
        session: &Path,
        mounts: &[Mount],
        network: Network,
        // Beside the reach rather than folded into it, because widening the one must never
        // widen the other -- see the boot's own note on the two axes.
        host_ports: &[u16],
        // How much of the host this machine gets, and whether it has a Vulkan device --
        // settled against this server's own defaults before it arrived here.
        shape: Shape,
    ) -> anyhow::Result<Machine> {
        use std::io::Read as _;
        use std::os::unix::fs::PermissionsExt as _;

        // The program a boot runs, signed when it was built -- see `halves`.
        let program = super::halves::boot()?;

        // Two directories, because two lifetimes. The session's is named by this process's
        // pid, which is what the sweep in `session` reads back, and holds the one thing that
        // outlives a machine -- the disk. `machine/` under it holds what only makes sense
        // while one is up, and is what a `stop` throws away.
        //
        // One name and not a counter per boot, because there is never more than one machine
        // here at a time: dropping the last one kills it, reaps it and removes this directory
        // before anything asks for another, all of it before the drop returns.
        let dir = super::sessions()
            .join(std::process::id().to_string())
            .join("machine");
        let guest_dir = dir.join("guest");
        std::fs::create_dir_all(&guest_dir)?;

        let (socket, console) = (dir.join("port.sock"), dir.join("console.log"));

        // A copy and not a link: this directory is served to the guest over virtio-fs, and
        // what it holds has to be a file under it rather than a name pointing out of it.
        let bin = guest_dir.join(GUEST_BIN);
        let from = super::halves::guest()?;
        std::fs::copy(&from, &bin)
            .map_err(|e| anyhow::anyhow!("copying {} into the share: {e}", from.display()))?;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755))?;

        // Bound before the child is started, because the child connects to it as its first
        // act and there is nothing to retry against if it is not there yet.
        let listener = std::os::unix::net::UnixListener::bind(&socket)
            .map_err(|e| anyhow::anyhow!("binding the console channel: {e}"))?;

        let mut told = told(
            image,
            session,
            &guest_dir,
            abin().as_deref(),
            &socket,
            network,
            host_ports,
            shape,
        );
        // In the order the client named them, which is the order the protocol says they are
        // realized in -- so a boot is handed the list as it stands and nothing here sorts or
        // merges it.
        for mount in mounts {
            told.push("--mount".into());
            told.push(mount.from.clone().into_os_string());
            told.push(
                if mount.read_only {
                    format!("{}:ro", mount.at)
                } else {
                    mount.at.clone()
                }
                .into(),
            );
        }

        // The guest's console -- kernel messages, a panic, whatever a boot that failed has to
        // say -- to a file rather than to this process's stderr, so that a machine which
        // never connected can still be asked what happened. It is the boot's own stdio
        // because that is where its `Session` puts the console; nothing in the boot opens a
        // log of its own.
        let log = std::fs::File::create(&console)
            .map_err(|e| anyhow::anyhow!("making {}: {e}", console.display()))?;
        let mut boot = std::process::Command::new(&program)
            .args(&told)
            .stdin(std::process::Stdio::null())
            .stdout(
                log.try_clone()
                    .map_err(|e| anyhow::anyhow!("sharing the console log: {e}"))?,
            )
            .stderr(log)
            .spawn()
            .map_err(|e| anyhow::anyhow!("starting {}: {e}", program.display()))?;

        // A virtio-console port discards what the host writes while no process in the guest
        // has it open, rather than queueing it -- the socket is up long before the kernel is.
        // So this waits for the guest to say that something is reading, and only then is
        // there a session.
        //
        // Waited for with a deadline and with an eye on the child, because the three ways
        // this does not happen are worth telling apart: a boot that could not start says so
        // by exiting, a guest that died inside says so the same way, and a machine that is
        // merely slow says nothing at all. Without this they are one hang.
        listener.set_nonblocking(true)?;
        let until = std::time::Instant::now() + BOOT_TIMEOUT;
        let (mut port, _) = loop {
            match listener.accept() {
                Ok(connected) => break connected,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if let Some(status) = boot.try_wait()? {
                        anyhow::bail!(
                            "the boot exited with {status} before the guest connected{}",
                            said(&console)
                        );
                    }
                    anyhow::ensure!(
                        std::time::Instant::now() < until,
                        "the guest did not connect within {}s{}",
                        BOOT_TIMEOUT.as_secs(),
                        said(&console)
                    );
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                Err(e) => anyhow::bail!("waiting for the guest to connect: {e}"),
            }
        };
        // Accepted from a non-blocking listener, which on some platforms is where the new
        // socket gets it from.
        port.set_nonblocking(false)?;
        port.set_read_timeout(Some(BOOT_TIMEOUT))?;

        let mut heard = [0u8; cortex_krun_host::HANDSHAKE.len()];
        port.read_exact(&mut heard).map_err(|e| {
            anyhow::anyhow!(
                "waiting for the guest to say it is there: {e}{}",
                said(&console)
            )
        })?;
        anyhow::ensure!(
            &heard == cortex_krun_host::HANDSHAKE,
            "the guest said {heard:?} where a handshake was expected"
        );

        // From here the channel is this end's to drive, so it moves to the runtime's socket
        // type: the framing below is async and the accepted one is not.
        port.set_nonblocking(true)?;
        let (incoming, outgoing) = tokio::net::UnixStream::from_std(port)?.into_split();

        Ok(Machine {
            boot,
            incoming: tokio::io::BufReader::new(incoming),
            outgoing,
            next: 0,
            dir,
        })
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
/// word where it had one and this server's environment where it did not -- so this spells what
/// it was handed and a machine's size is answered in one place rather than two.
fn told(
    image: &str,
    session: &Path,
    guest: &Path,
    // Taken rather than looked up here, so that what a boot is told is a function of its
    // arguments: `abin()` asks the filesystem, and the one caller asks it once.
    abin: Option<&Path>,
    channel: &Path,
    network: Network,
    host_ports: &[u16],
    shape: Shape,
) -> Vec<OsString> {
    let mut told: Vec<OsString> = vec![
        "--image".into(),
        image.into(),
        "--session".into(),
        session.into(),
        "--guest".into(),
        guest.into(),
        "--channel".into(),
        channel.into(),
        // Always spelled, even for a session that reaches nothing: the boot requires it, so
        // a reach cannot be lost between here and there without the boot saying so.
        "--network".into(),
        network.as_str().into(),
    ];
    // Left off entirely when this host has none, which is what the boot reads as a machine
    // that gets no `/abin` rather than one with an empty directory at that path.
    if let Some(abin) = abin {
        told.extend(["--abin".into(), abin.into()]);
    }
    for port in host_ports {
        told.extend(["--host-port".into(), OsString::from(port.to_string())]);
    }
    if let Some(vcpus) = shape.vcpus {
        told.extend(["--vcpus".into(), OsString::from(vcpus.to_string())]);
    }
    if let Some(memory) = shape.memory_mib {
        told.extend(["--memory-mib".into(), OsString::from(memory.to_string())]);
    }
    if shape.gpu {
        told.push("--gpu".into());
    }
    told
}

/// Cortex's own executables, or `None` for a host that has none.
///
/// One well-known place under [`home`](crate::home) and no variable naming another: where
/// this server keeps things is already something `home` answers, so a second way to point
/// somewhere else would be two answers to one question.
///
/// **A host with none is a session with no `/abin`, not a session that fails.** These are
/// executables a session is better off with and can run without -- see `ABIN_DIR`.
fn abin() -> Option<PathBuf> {
    let dir = crate::home().join("abin");
    dir.is_dir().then_some(dir)
}

/// A default read out of this server's environment, ignoring anything that is not a number.
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
                Path::new("/s/port.sock"),
                Network::Disabled,
                &[],
                Shape::default(),
            )
            .into_iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
        };

        assert!(!line(None).iter().any(|arg| arg == "--abin"));

        let given = line(Some(Path::new("/h/abin")));
        let at = given.iter().position(|arg| arg == "--abin").unwrap();
        assert_eq!(given[at + 1], "/h/abin");
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
                Path::new("/s/port.sock"),
                Network::Disabled,
                &[],
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

        let asked = line(Shape {
            vcpus: Some(4),
            memory_mib: Some(4096),
            gpu: true,
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
    }
}
