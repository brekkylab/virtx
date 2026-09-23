//! A micro-VM, booted on an image this server made sure was in the cache.
//!
//! **Nothing here holds a hypervisor.** What this module does is make what a machine needs on
//! disk, spawn `cortex-krun-host` over it, and talk to the guest down a socket that boot
//! turned into a console port. The VMM and the entitlement it needs are both in that other
//! binary -- see [`halves`](super::halves) for where it and the agent are found.
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
use cortex_krun_host::AGENT_BIN;

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

/// Guest vCPUs and memory, when this server was told to override what a boot would otherwise
/// pick.
///
/// Read here and passed on the command line, rather than left for the boot to find in the
/// environment it inherits: what a machine was given is then something this end can be asked,
/// and the boot has one place it learns its shape from.
const VCPUS_ENV: &str = "CORTEX_KRUN_VCPUS";
const MEMORY_ENV: &str = "CORTEX_KRUN_MEMORY_MIB";

/// Whether to ask for a Vulkan device in the guest.
///
/// This server's own setting and not the client's, because whether it can be given is a fact
/// about the machine this runs on -- the boot has to have been built with `gpu`, and that
/// build needs a virglrenderer installed here. A client has no way to know either.
const GPU_ENV: &str = "CORTEX_KRUN_GPU";

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
        mounts: &[Mount],
        network: Network,
        // Beside the reach rather than folded into it, because widening the one must never
        // widen the other -- see the boot's own note on the two axes.
        host_ports: &[u16],
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
        let owner = super::sessions().join(std::process::id().to_string());
        let dir = owner.join("machine");
        let agent_dir = dir.join("agent");
        std::fs::create_dir_all(&agent_dir)?;

        let (socket, console) = (dir.join("port.sock"), dir.join("console.log"));

        // A copy and not a link: this directory is served to the guest over virtio-fs, and
        // what it holds has to be a file under it rather than a name pointing out of it.
        let agent = agent_dir.join(AGENT_BIN);
        let from = super::halves::guest()?;
        std::fs::copy(&from, &agent)
            .map_err(|e| anyhow::anyhow!("copying {} into the share: {e}", from.display()))?;
        std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o755))?;

        // Bound before the child is started, because the child connects to it as its first
        // act and there is nothing to retry against if it is not there yet.
        let listener = std::os::unix::net::UnixListener::bind(&socket)
            .map_err(|e| anyhow::anyhow!("binding the console channel: {e}"))?;

        let mut told = told(
            image,
            &owner.join("session.ext4"),
            &agent_dir,
            &socket,
            network,
            host_ports,
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
/// argument it does not know says so and exits before a hypervisor is touched. The three
/// overrides are this server's own environment rather than anything a client said, because
/// what they are worth is a fact about the machine this runs on.
fn told(
    image: &str,
    session: &Path,
    agent: &Path,
    channel: &Path,
    network: Network,
    host_ports: &[u16],
) -> Vec<OsString> {
    let mut told: Vec<OsString> = vec![
        "--image".into(),
        image.into(),
        "--session".into(),
        session.into(),
        "--agent".into(),
        agent.into(),
        "--channel".into(),
        channel.into(),
        // Always spelled, even for a session that reaches nothing: the boot requires it, so
        // a reach cannot be lost between here and there without the boot saying so.
        "--network".into(),
        network.as_str().into(),
    ];
    for port in host_ports {
        told.extend(["--host-port".into(), OsString::from(port.to_string())]);
    }
    if let Some(vcpus) = number::<u8>(VCPUS_ENV) {
        told.extend(["--vcpus".into(), OsString::from(vcpus.to_string())]);
    }
    if let Some(memory) = number::<u32>(MEMORY_ENV) {
        told.extend(["--memory-mib".into(), OsString::from(memory.to_string())]);
    }
    if flag(GPU_ENV) {
        told.push("--gpu".into());
    }
    told
}

/// An override read out of this server's environment, ignoring anything that is not a number.
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
