//! `cortex-krun-host` -- assemble the micro-VM a server described, and enter it.
//!
//! This never returns. [`Session::enter`] hands the process to the VMM, and when the guest
//! shuts down the VMM exits the process with the guest's own status -- so whatever runs a VM
//! *becomes* a VM and stops being able to do anything else. That is the first reason a boot
//! is a child process: a server has a session to keep answering, and it cannot be the thing
//! that disappears when the guest does.
//!
//! The second is the signature. Creating a VM through Hypervisor.framework needs an
//! entitlement, an entitlement is carried by a code signature, and a signature is read at
//! `exec` -- so the process that creates a VM can never be the process that decided to. This
//! binary is signed where it is built and spawned as a file beside the server; what gets
//! signed is therefore this, which links a VMM and nothing else, rather than the server,
//! which links an image store and a console.
//!
//! # The command line
//!
//! ```text
//! --image <ref>         what the session runs; already in the cache, because a boot asks
//!                       no registry anything
//! --session <path>      the ext4 the session writes to -- made here if it is not there
//! --guest <dir>         shared into the guest at GUEST_DIR; holds the guest binary
//! --abin <dir>          shared read-only at ABIN_DIR; cortex's own executables, and the
//!                       directory that goes first on a command's PATH          [optional]
//! --channel <path>      the socket to connect back on, which becomes the console port
//! --network <reach>     none | host | public | full; what the guest can reach
//! --host-port <n>       a TCP port on this machine the guest may open, repeatable [optional]
//! --mount <dir> <at>    a tree to share into the guest: the host directory, then the
//!                       absolute path it appears at, with `:ro` on the end of that path
//!                       for a tree a command may not write in. Repeatable, and the
//!                       shares are made in the order given -- which is what puts one
//!                       tree inside another                                  [optional]
//! --vcpus <n>           overrides, when the server was told to                [optional]
//! --memory-mib <n>                                                            [optional]
//! --gpu                 attach virtio-gpu; refused unless built with `gpu`   [optional]
//! ```
//!
//! And one argument that is a question rather than a machine:
//!
//! ```text
//! --capabilities        write what this build can make, a name to a line, and exit
//! ```
//!
//! Read here and spelled where a boot is spawned, with no type shared between the two. A boot
//! is started by `cortex-krun` and by nothing else, the two are built by one `cargo build`,
//! and an argument either end gets wrong is refused below -- by name, before a hypervisor is
//! touched. Nothing is decided here that the server did not already decide: this binary
//! exists to hold a hypervisor, not to have opinions.
//!
//! # What it assembles, and what assembles it
//!
//! [`Session`], which is the same machine `cargo run --example run` boots: the image's disk
//! as the overlay's lower, the session's ext4 as its upper, the stacking done in the guest by
//! libkrun's own init. Three things are added to it, and they are the whole of what a console
//! session is:
//!
//! ```text
//! a console port   the protocol's channel, the other end of it a socket on the server
//! the guest share  the directory the guest binary is in, so the guest has one to exec
//! an exact argv    the guest binary, and not whatever the image states as its program
//! ```
//!
//! And one more when the server has it to give: `/abin`, cortex's own executables, shared
//! read-only. Optional because it is the one of these a session can do without -- see
//! [`ABIN_DIR`].
//!
//! The trees a session was given are ordinary shares beside them, each read-only or not as
//! the client asked, mounted by the init once the root is the overlay.
//!
//! # The console, and where it goes
//!
//! To this process's own stdio, which the server redirects to a file before spawning it.
//! Nothing here opens one: a machine that never came up has to be readable afterwards, and
//! where that reading happens is the server's arrangement rather than a device's.

use std::convert::Infallible;
use std::ffi::OsString;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::ExitCode;

use cortex_krun::session::{Ext4, Mount, Network, Session};
use cortex_krun_host::{ABIN_DIR, CAPABILITIES_ARG, GUEST_DIR, PORT_NAME, guest_program};

/// A failure of ours, not a guest's -- the shell's code for "found it, could not run it",
/// which is what a boot that could not become a VM is.
const NOT_EXECUTABLE: u8 = 126;

/// What this build can make, for the server that is about to spawn it -- see
/// [`CAPABILITIES_ARG`].
///
/// A capability is here when a feature decides whether an argument can be honoured, which
/// today is one: `--gpu` needs a device that needs virglrenderer, and a host without that
/// library does not link a build that has it. Everything else on the command line is answered
/// by every build of this binary, and a name for those would be a list a server reads and
/// learns nothing from.
const CAPABILITIES: &[&str] = &[
    #[cfg(feature = "gpu")]
    cortex_krun_host::GPU_CAPABILITY,
];

fn main() -> ExitCode {
    let argv: Vec<OsString> = std::env::args_os().skip(1).collect();

    // Answered before the argv is read as a machine, because it is not one: a server asks
    // this of the file it is about to spawn, and nothing else on the line is meaningful
    // beside it. Alone, therefore -- a boot asked what it can do and also handed a session
    // is two commands, and the second would never run.
    if argv.first().is_some_and(|first| first == CAPABILITIES_ARG) {
        if argv.len() > 1 {
            eprintln!(
                "{}: {CAPABILITIES_ARG} is asked on its own",
                env!("CARGO_BIN_NAME")
            );
            return ExitCode::from(NOT_EXECUTABLE);
        }
        for name in CAPABILITIES {
            println!("{name}");
        }
        return ExitCode::SUCCESS;
    }

    cortex_krun::session::log_from_env();

    match Args::parse(argv).and_then(enter) {
        // `enter` only returns on success by not returning at all: the `Ok` holds an
        // `Infallible`, and the empty match is what says so.
        Ok(never) => match never {},
        Err(e) => {
            eprintln!("{}: {e:#}", env!("CARGO_BIN_NAME"));
            ExitCode::from(NOT_EXECUTABLE)
        }
    }
}

/// Everything the server said, as this binary reads it back -- see the module doc.
struct Args {
    image: String,
    session: PathBuf,
    guest: PathBuf,
    /// The directory holding cortex's own executables, and `None` for a server that had
    /// none to give -- see [`ABIN_DIR`]. Optional where `guest` is required: a machine
    /// with no guest binary has nothing to exec, and one with no `/abin` merely has less on
    /// `PATH`.
    abin: Option<PathBuf>,
    channel: PathBuf,
    /// How much of the network the session gets, and which doors on this machine are open
    /// to it. Two axes rather than one: `network` says how far out a session goes, and the
    /// ports say what it may reach *here* -- so widening the one never widens the other.
    network: Network,
    host_ports: Vec<u16>,
    /// The trees the session was given, in the order the client named them -- see
    /// [`Share`].
    mounts: Vec<Share>,
    vcpus: Option<u8>,
    memory_mib: Option<u32>,
    gpu: bool,
}

impl Args {
    /// Read the command line, refusing anything this does not recognise.
    ///
    /// A boot is started by a console server and by nothing else, so a missing argument is
    /// the two halves of one repository disagreeing rather than a caller to be lenient with.
    /// It is refused here, which is the cheapest place it can be: before a socket is
    /// connected and long before a hypervisor is asked for anything.
    fn parse(argv: impl IntoIterator<Item = OsString>) -> anyhow::Result<Args> {
        let mut image = None;
        let mut session = None;
        let mut guest = None;
        let mut abin = None;
        let mut channel = None;
        let mut network = None;
        let mut host_ports = Vec::new();
        let mut mounts = Vec::new();
        let mut vcpus = None;
        let mut memory_mib = None;
        let mut gpu = false;

        let mut argv = argv.into_iter();
        while let Some(flag) = argv.next() {
            let flag = flag
                .into_string()
                .map_err(|flag| anyhow::anyhow!("{flag:?} is not an argument this reads"))?;
            // `OsString` for the paths: one that is not UTF-8 is still a path, and nothing
            // here has to read one as text. A mount point is the exception -- it is told to
            // the guest as a string -- and that is read as one where it arrives.
            let mut value = || {
                argv.next()
                    .ok_or_else(|| anyhow::anyhow!("{flag} was given nothing to be"))
            };
            match flag.as_str() {
                "--image" => image = Some(text(&flag, value()?)?),
                "--session" => session = Some(PathBuf::from(value()?)),
                "--guest" => guest = Some(PathBuf::from(value()?)),
                "--abin" => abin = Some(PathBuf::from(value()?)),
                "--channel" => channel = Some(PathBuf::from(value()?)),
                "--network" => {
                    let name = text(&flag, value()?)?;
                    network = Some(Network::parse(&name).ok_or_else(|| {
                        anyhow::anyhow!("--network: {name} is not none, host, public or full")
                    })?);
                }
                "--host-port" => host_ports.push(number(&flag, value()?)?),
                // Two values, because a mount is two things this binary reads differently:
                // a host directory, which is a path and need not be text, and the place it
                // takes in the guest, which is a string the protocol already spells.
                "--mount" => {
                    let from = PathBuf::from(value()?);
                    mounts.push(Share::new(from, &text(&flag, value()?)?)?);
                }
                "--vcpus" => vcpus = Some(number(&flag, value()?)?),
                "--memory-mib" => memory_mib = Some(number(&flag, value()?)?),
                // A flag and not a value: it is asked for or it is not, and `--gpu false`
                // would be a second spelling of leaving it out.
                //
                "--gpu" => gpu = true,
                other => anyhow::bail!("{other} is not an argument a boot takes"),
            }
        }

        // Refused where this build has no device to attach, rather than taken and dropped by
        // the session layer: a session that asked for an accelerator and quietly ran without
        // one is a client waiting on the CPU for an answer nothing told it was coming that
        // way. A server asks `--capabilities` before it spells this, so reaching here is a
        // boot named by `CORTEX_KRUN_BOOT_BIN` that is not the build the server asked.
        #[cfg(not(feature = "gpu"))]
        anyhow::ensure!(
            !gpu,
            "--gpu: this boot was built without the `gpu` feature, which is what virglrenderer \
             and a virtio-gpu device come in with"
        );

        let required = |name: &str, value: Option<PathBuf>| {
            value.ok_or_else(|| {
                anyhow::anyhow!("{name} is missing -- a boot is started by a server")
            })
        };
        Ok(Args {
            image: image
                .ok_or_else(|| anyhow::anyhow!("--image is missing -- a boot runs one image"))?,
            session: required("--session", session)?,
            guest: required("--guest", guest)?,
            abin,
            channel: required("--channel", channel)?,
            // Required rather than defaulted to `none`. A reach is the server's decision and
            // the server always spells it, so a missing one is not a session that wanted no
            // network -- it is one whose reach was dropped on the way here, and a sandbox
            // quietly narrower than it was asked to be is worse than one that would not start.
            network: network.ok_or_else(|| {
                anyhow::anyhow!("--network is missing -- a boot is told its reach")
            })?,
            host_ports,
            mounts,
            vcpus,
            memory_mib,
            gpu,
        })
    }
}

/// One tree the guest gets: a directory on this side, the absolute path it appears at, and
/// whether a command may write in it.
///
/// The mount point carries its own `ro` rather than travelling as a third value, because the
/// protocol's own spelling already puts it there -- a guest path is absolute and carries no
/// colon, so a trailing `:ro` is unambiguous without anything having to be quoted.
struct Share {
    from: PathBuf,
    at: String,
    read_only: bool,
}

impl Share {
    /// Read `<path>[:ro]` against the directory it names, refusing a mount point this binary
    /// cannot pass on as one.
    fn new(from: PathBuf, at: &str) -> anyhow::Result<Share> {
        let (at, read_only) = match at.strip_suffix(":ro") {
            Some(at) => (at, true),
            None => (at.strip_suffix(":rw").unwrap_or(at), false),
        };
        anyhow::ensure!(
            at.starts_with('/'),
            "--mount: {at} is not an absolute path to appear at"
        );
        Ok(Share {
            from,
            at: at.to_string(),
            read_only,
        })
    }
}

/// An argument's value as text, for the ones that are not a path.
fn text(flag: &str, value: OsString) -> anyhow::Result<String> {
    value
        .into_string()
        .map_err(|value| anyhow::anyhow!("{flag} is not utf-8: {value:?}"))
}

/// An argument's value as a number. Refused rather than defaulted: a server computed it, so a
/// value that is not one is a bug on this side of the process boundary.
fn number<T: std::str::FromStr>(flag: &str, value: OsString) -> anyhow::Result<T> {
    text(flag, value)?
        .parse()
        .map_err(|_| anyhow::anyhow!("{flag} is not a number"))
}

fn enter(args: Args) -> anyhow::Result<Infallible> {
    // The console port is a descriptor, and this is where it comes from: one connection back
    // to the process that spawned this one. Held for the length of this function, which is
    // the length of the process -- the VM borrows it for as long as it runs.
    let channel = UnixStream::connect(&args.channel).map_err(|e| {
        anyhow::anyhow!(
            "connecting to the console channel at {}: {e}",
            args.channel.display()
        )
    })?;

    let mut session = Session::new()
        .image(&args.image)
        // The disk is the session. It is made here and not by the server because this is the
        // process that is about to open it, and a boot that could not format one is a boot
        // that failed rather than a session the server thought it had.
        .rootfs(Ext4::at(&args.session, Ext4::SIZE)?)
        .port(PORT_NAME, channel.as_raw_fd())
        // The directory the guest binary is in, shared so that the guest has one to exec.
        .mount(Mount::new(&args.guest, GUEST_DIR))
        .network(args.network)
        .host_ports(args.host_ports.iter().copied())
        // Exactly the guest binary -- see [`Session::exec`]. An image stating an entrypoint
        // states it about its own program, and this is not that program.
        .exec([guest_program()])
        .gpu(args.gpu);

    // Cortex's own executables, and the ordinary mount path rather than anything of its own:
    // `read_only` here is the same promise it is for a tree a caller handed over -- the
    // host's virtiofs server refuses the write, so a guest that is root inside itself cannot
    // remount it away. Before the client's trees below, because it is this backend's.
    if let Some(abin) = &args.abin {
        session = session.mount(Mount::new(abin, ABIN_DIR).read_only());
    }

    if let Some(vcpus) = args.vcpus {
        session = session.vcpus(vcpus);
    }
    if let Some(memory) = args.memory_mib {
        session = session.ram_mib(memory);
    }
    // In the order they were given, which is the order the protocol says they are realized
    // in: a share at `/work/out` that follows one at `/work` lands inside it.
    //
    // A read-only tree is read-only *at the device*: the host's virtiofs server refuses
    // every write, whatever the guest mounted it with, which a guest that is root inside
    // itself could otherwise remount away. That is what lets a caller hand over somebody's
    // project and get it back unchanged rather than a promise that nothing touched it.
    for share in &args.mounts {
        let mount = Mount::new(&share.from, &share.at);
        session = session.mount(if share.read_only {
            mount.read_only()
        } else {
            mount
        });
    }

    Ok(session.enter()?)
}
