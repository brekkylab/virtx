//! A machine, from both ends: the process that becomes one, and the one that spawns it.
//!
//! # The parent: [`start`] and [`sync`]
//!
//! What a session and a build both do with a machine, and nothing either does alone: spawn
//! this binary again as `virtx-uvm boot`, tell the guest what its session is, and ask it to
//! flush before the machine is taken down. A build runs its script between the two, and a
//! session answers its client there.
//!
//! # The child: [`enter`]
//!
//! `virtx-uvm boot` assembles the [`Machine`] the parent described, and enters it.
//!
//! This never returns. [`Machine::enter`] hands the process to the VMM, and when the guest
//! shuts down the VMM exits the process with the guest's own status -- so whatever runs a VM
//! *becomes* a VM and stops being able to do anything else. That is why a boot is a child
//! process: a session has a client to keep answering, and it cannot be the thing that
//! disappears when the guest does.
//!
//! And on macOS a VM needs the hypervisor entitlement, which is read from the signature at
//! `exec` -- so the boot is a fresh `exec` of this same, signed, binary rather than a thread
//! of the process that decided to make one.
//!
//! The command line is [`Args`], spelled by `virtx_uvm::boot`'s `told` and read here. A boot
//! is started by this binary and by nothing else, so an argument either end gets wrong is
//! refused by name, before a hypervisor is touched.
//!
//! What it assembles is the image's root filesystem as the overlay's lower and the session's
//! ext4 as its upper, and three things that are the whole of what a console session is:
//!
//! ```text
//! a console port   the protocol's channel, the other end of it a socket on the parent
//! the guest share  the directory the guest binary is in, so the guest has one to exec
//! an exact argv    the guest binary, and not whatever the image states as its program
//! ```
//!
//! And `/abin` when the parent has it to give, and the trees the session was given, each an
//! ordinary share mounted by the init once the root is the overlay.
//!
//! The guest's console goes to this process's own stdio, which the parent redirects to a file
//! before spawning it: a machine that never came up has to be readable afterwards.

use std::convert::Infallible;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use virtx::protocol::{Call, ExecCall, InitCall, MountSpec, Response};
use virtx_uvm::boot::{Boot, Doors, Shape};
use virtx_uvm::cache::Cache;
use virtx_uvm::filesystem::Tree;
use virtx_uvm::image::Image;
use virtx_uvm::machine::doors::sessions;
use virtx_uvm::machine::{Channel, Machine, Mount, Upper};
use virtx_uvm::registry::Reference;
use virtx_uvm::rootfs::Rootfs;

// The names this end and the guest both spell, and must spell the same: the port, the
// handshake, and where `/abin` is. Written in both places -- the guest's are in its `main.rs`
// -- so a change to one is a change to the other.

/// The virtio-console port carrying the console session.
///
/// The guest's *console* is the kernel's -- it is where a panic and every `printk` land -- so
/// the protocol runs on a named port beside it rather than on the console itself, which a
/// single kernel warning would corrupt.
const PORT_NAME: &str = "virtx-console";

/// Sent by the guest, once, as soon as it has the port open, and consumed here before the
/// first frame goes out: a virtio-console port discards what the host writes while no process
/// in the guest has it open, so the guest has to say that something is reading.
const HANDSHAKE: &[u8; 8] = b"VIRTXUVM";

/// Where virtx's own executables are mounted inside the guest, and what goes first on a
/// command's `PATH`. Not a dotted name, unlike [`GUEST_DIR`]: these are programs a session is
/// meant to run by name. A session whose host has none gets no `/abin` at all.
pub(crate) const ABIN_DIR: &str = "/abin";

/// Where the machine's own directory is mounted inside the guest, and therefore where the
/// guest binary is found. A share rather than anything built into the image: the guest
/// binary is this build's, and the image is whoever's the session named. Dotted and at the
/// root, out of the way of whatever the image put there.
pub(crate) const GUEST_DIR: &str = "/.virtx";

/// The guest binary's file name, beside this binary and inside [`GUEST_DIR`].
const GUEST_BIN: &str = "virtx-uvm-guest";

/// What the guest execs: its own binary, at the one path both ends spell.
fn guest_program() -> String {
    format!("{GUEST_DIR}/{GUEST_BIN}")
}

/// What the guest is asked to run before a machine is taken down -- see the guest's own note
/// on it. The disk is read on this side, and a machine is stopped by killing it.
const SYNC: &str = "sync";

/// Boot `image` over `upper`, and tell the guest what its session is.
///
/// Answers with where the session stands, which is the image's business and only the guest
/// has read it.
pub(crate) async fn start(
    image: &str,
    // The writable disk the overlay's upper layer goes on: a session's is kept and carried
    // on, a build's is read once and thrown away.
    upper: &Path,
    mounts: &[Mount],
    network: bool,
    doors: &Doors,
    shape: Shape,
) -> anyhow::Result<(Boot, Option<String>)> {
    // This binary again, as `boot`: the process that becomes the machine.
    let program =
        std::env::current_exe().map_err(|e| anyhow::anyhow!("asking where this binary is: {e}"))?;
    let abin = abin();

    let mut boot = Boot::start(
        &program,
        &["boot".into()],
        &guest()?,
        GUEST_BIN,
        HANDSHAKE,
        abin.as_deref(),
        // One machine at a time per process: dropping the last one removes this directory
        // before anything asks for another.
        &sessions()
            .join(std::process::id().to_string())
            .join("machine"),
        image,
        upper,
        mounts,
        network,
        doors,
        shape,
    )
    .await?;

    // The guest answers nothing until it has been told what the session is. Only the mounts
    // reach it, as the paths the boot already shared them at: an image is what was booted,
    // a network is a device that either is or is not attached by now, and the machine's
    // shape was spent on the boot.
    let told = InitCall {
        mounts: mounts
            .iter()
            .map(|mount| {
                let at = mount.at.to_str().ok_or_else(|| {
                    anyhow::anyhow!("{} is not a path the guest can be told", mount.at.display())
                })?;
                let spec = MountSpec::new(format!("file://{at}"), at)?;
                Ok(if mount.read_only {
                    spec.read_only()
                } else {
                    spec
                })
            })
            .collect::<anyhow::Result<_>>()?,
        network: true,
        ..InitCall::default()
    };
    match boot.call(Call::Init(told)).await? {
        Response::Init(answer) => Ok((boot, answer.cwd)),
        Response::Error(e) => anyhow::bail!("the guest refused the session: {}", e.message),
        other => anyhow::bail!("the guest answered an init with {other:?}"),
    }
}

/// Ask the guest to put what it wrote on the disk, before the machine is killed.
///
/// What the guest wrote is on its way to the disk and not necessarily on it; a machine is
/// stopped by killing it, which flushes nothing; and the disk is about to be read on this
/// side.
pub(crate) async fn sync(boot: &mut Boot) -> anyhow::Result<()> {
    match boot
        .call(Call::Exec(ExecCall {
            cmd: vec![guest_program(), SYNC.into()],
            timeout_ms: None,
        }))
        .await?
    {
        Response::Exec(done) if done.code == 0 => Ok(()),
        Response::Exec(done) => anyhow::bail!("flushing the disk exited {}", done.code),
        Response::Error(e) => anyhow::bail!("flushing the disk: {}", e.message),
        other => anyhow::bail!("the guest answered a flush with {other:?}"),
    }
}

/// Whether a boot of this binary can attach a GPU here: built with `gpu`, on a host with a
/// Vulkan loader for venus to reach.
///
/// **Known rather than asked.** The boot is this same file, so what it was built with is what
/// this process was built with -- and the loader is the host's, found the way the boot will
/// look for it.
pub(crate) fn can_gpu() -> bool {
    #[cfg(feature = "gpu")]
    return vulkan::loader().is_ok();
    #[cfg(not(feature = "gpu"))]
    return false;
}

/// The guest binary: `$VIRTX_UVM_GUEST_BIN`, else `virtx-uvm-guest` beside this binary.
///
/// No suffix, whatever this host puts on one of its own: these bytes are a static musl ELF
/// that runs inside the VM, and the name is one a Linux guest is about to exec.
fn guest() -> anyhow::Result<PathBuf> {
    if let Some(named) = std::env::var_os("VIRTX_UVM_GUEST_BIN") {
        let path = PathBuf::from(named);
        anyhow::ensure!(
            path.is_file(),
            "VIRTX_UVM_GUEST_BIN={} is not a file",
            path.display()
        );
        return Ok(path);
    }
    let exe =
        std::env::current_exe().map_err(|e| anyhow::anyhow!("asking where this binary is: {e}"))?;
    let path = exe
        .parent()
        .ok_or_else(|| anyhow::anyhow!("{} has no directory to look in", exe.display()))?
        .join(GUEST_BIN);
    anyhow::ensure!(
        path.is_file(),
        "no guest binary at {} (set VIRTX_UVM_GUEST_BIN to name one)",
        path.display()
    );
    Ok(path)
}

/// Virtx's own executables, or `None` for a host that has none: `abin/` beside this binary
/// first, and then `abin/` under the cache root.
///
/// **A host with none is a machine with no `/abin`, not one that fails.**
fn abin() -> Option<PathBuf> {
    let beside = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("abin")));
    beside
        .into_iter()
        .chain([virtx::cache_root().join("abin")])
        .find(|dir| dir.is_dir())
}

/// Everything the parent said, as `boot` reads it back -- see the module doc.
#[derive(clap::Args)]
pub(crate) struct Args {
    /// What the session runs; already in the cache, because a boot asks no registry anything
    /// -- its root filesystem is made here if it has not been yet.
    #[arg(long)]
    image: String,
    /// The ext4 the session writes to -- made here if it is not there.
    #[arg(long)]
    session: PathBuf,
    /// Shared into the guest at `GUEST_DIR`; holds the guest binary.
    #[arg(long)]
    guest: PathBuf,
    /// Shared read-only at `ABIN_DIR`: virtx's own executables, and the directory that goes
    /// first on a command's `PATH`. Optional where `guest` is required: a machine with no
    /// guest binary has nothing to exec, and one with no `/abin` merely has less on `PATH`.
    #[arg(long)]
    abin: Option<PathBuf>,
    /// The socket to connect back on, which becomes the console port.
    #[arg(long)]
    channel: PathBuf,
    /// Give the guest a network; without it, it has none.
    #[arg(long)]
    network: bool,
    /// A guest port this machine may connect into, only beside `--network` -- see [`relay`].
    #[arg(long)]
    publish: Vec<u16>,
    /// Where to say which loopback port each `--publish` is at; beside `--publish` and only
    /// there.
    #[arg(long)]
    relays: Option<PathBuf>,
    /// A tree to share into the guest: the host directory, then the absolute path it appears
    /// at, with `:ro` on the end of that path for a tree a command may not write in. The
    /// shares are made in the order given -- which is what puts one tree inside another.
    #[arg(long, num_args = 2, value_names = ["DIR", "AT"])]
    mount: Vec<OsString>,
    #[arg(long)]
    vcpus: Option<u8>,
    #[arg(long)]
    memory_mib: Option<u32>,
    /// Attach virtio-gpu; refused unless built with `gpu`.
    #[arg(long)]
    gpu: bool,
    /// What that GPU may hold -- its host-visible window and the heap the guest sees; only
    /// beside `--gpu`.
    #[arg(long)]
    gpu_memory_mib: Option<u32>,
}

/// Assemble the machine `args` describes, and become it.
///
/// `enter` only returns on success by not returning at all: the `Ok` holds an [`Infallible`].
pub(crate) fn enter(args: Args) -> anyhow::Result<Infallible> {
    virtx_uvm::machine::log_from_env();

    let mounts = shares(&args.mount)?;
    checked(&args)?;

    // The console port is a descriptor, and this is where it comes from: one connection back
    // to the process that spawned this one. Held for the length of this function, which is
    // the length of the process -- the VM borrows it for as long as it runs.
    let channel = Channel::connect(&args.channel).map_err(|e| {
        anyhow::anyhow!(
            "connecting to the console channel at {}: {e}",
            args.channel.display()
        )
    })?;

    let (image, rootfs) = provision(&args.image)?;

    let mut machine = Machine::new(rootfs, image.config)
        // The disk is the session. It is made here and not by the parent because this is the
        // process that is about to open it, and a boot that could not format one is a boot
        // that failed rather than a session the parent thought it had.
        .upper(match Upper::open(&args.session) {
            Some(upper) => upper,
            None => Upper::create(&args.session, Upper::SIZE, Tree::new())?,
        })
        .port(PORT_NAME, &channel)
        // The directory the guest binary is in, shared so that the guest has one to exec.
        .mount(Mount::new(&args.guest, GUEST_DIR))
        .network(args.network)
        // Exactly the guest binary -- see [`Machine::exec`]. An image stating an entrypoint
        // states it about its own program, and this is not that program.
        .exec([guest_program()])
        .gpu(args.gpu);

    // Virtx's own executables, and the ordinary mount path rather than anything of its own:
    // `read_only` here is the same promise it is for a tree a caller handed over -- the
    // host's virtiofs server refuses the write, so a guest that is root inside itself cannot
    // remount it away. Before the client's trees below, because it is this backend's.
    if let Some(abin) = &args.abin {
        machine = machine.mount(Mount::new(abin, ABIN_DIR).read_only());
    }

    if let Some(vcpus) = args.vcpus {
        machine = machine.vcpus(vcpus);
    }
    if let Some(memory) = args.memory_mib {
        machine = machine.ram_mib(memory);
    }
    if let Some(memory) = args.gpu_memory_mib {
        machine = machine.gpu_memory_mib(memory);
    }
    // In the order they were given, which is the order the protocol says they are realized
    // in: a share at `/work/out` that follows one at `/work` lands inside it.
    //
    // A read-only tree is read-only *at the device*: the host's virtiofs server refuses
    // every write, whatever the guest mounted it with, which a guest that is root inside
    // itself could otherwise remount away.
    machine = machine.mounts(mounts);

    // Last before the machine, because what it writes is the parent's signal that the doors
    // are there: once the guest connects, the file has been written -- see [`relay`].
    if let Some(relays) = &args.relays {
        for (console, listener) in relay(&args.publish, relays)? {
            machine = machine.publish(console, listener);
        }
    }

    Ok(machine.enter()?)
}

/// What clap cannot say about the command line: the arguments that only make sense together.
fn checked(args: &Args) -> anyhow::Result<()> {
    // Refused where this build has no device to attach, rather than taken and dropped by the
    // machine: a session that asked for an accelerator and quietly ran without one is a
    // client waiting on the CPU for an answer nothing told it was coming that way.
    #[cfg(not(feature = "gpu"))]
    anyhow::ensure!(
        !args.gpu,
        "--gpu: this binary was built without the `gpu` feature, which is what virglrenderer \
         and a virtio-gpu device come in with"
    );
    // And where this build has one but the host has no Vulkan for it, for the same reason --
    // and because what the device would say instead is a guest with no GPU in it.
    #[cfg(feature = "gpu")]
    if args.gpu {
        vulkan::loader().map_err(|e| anyhow::anyhow!("--gpu: {e}"))?;
    }

    // A size of a device that is not there: the parent resolves the two together, so a boot
    // told one without the other is the halves disagreeing.
    anyhow::ensure!(
        args.gpu || args.gpu_memory_mib.is_none(),
        "--gpu-memory-mib without --gpu -- a size is of a GPU"
    );

    // A port is carried in over the network, so one without it is the halves disagreeing.
    anyhow::ensure!(
        args.network || args.publish.is_empty(),
        "--publish without --network -- a published port is reached over the network"
    );

    // Both or neither: a door nobody is told the way to is one no connection can find, and a
    // file saying where no door is is the halves disagreeing about which boot this is.
    anyhow::ensure!(
        args.publish.is_empty() == args.relays.is_none(),
        "--publish and --relays come together -- a published port is one the parent is told \
         the way to"
    );
    Ok(())
}

/// The `--mount` pairs, each read as `<dir> <at>[:ro]`.
///
/// The mount point carries its own `ro` rather than travelling as a third value, because the
/// protocol's own spelling already puts it there -- a guest path is absolute and carries no
/// colon, so a trailing `:ro` is unambiguous without anything having to be quoted.
fn shares(pairs: &[OsString]) -> anyhow::Result<Vec<Mount>> {
    pairs
        .chunks(2)
        .map(|pair| {
            let [host, at] = pair else {
                anyhow::bail!("--mount takes a directory and where it appears");
            };
            // A host directory is a path and need not be text; the place it takes in the
            // guest is a string the protocol already spells.
            let at = at
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("--mount: {at:?} is not utf-8"))?;
            let (at, read_only) = match at.strip_suffix(":ro") {
                Some(at) => (at, true),
                None => (at.strip_suffix(":rw").unwrap_or(at), false),
            };
            anyhow::ensure!(
                at.starts_with('/'),
                "--mount: {at} is not an absolute path to appear at"
            );
            let mount = Mount::new(host, at);
            Ok(if read_only { mount.read_only() } else { mount })
        })
        .collect()
}

/// The image `named` names in the cache, and its root filesystem -- made now if this is the
/// first boot of those layers.
///
/// From the cache and nowhere else: a boot asks no registry anything, so an image that was
/// never pulled is a failure to report rather than a download to start. Made here rather than
/// by the parent because it is a disk this process is about to open, as the session's own is.
fn provision(named: &str) -> anyhow::Result<(Image, Rootfs)> {
    let reference: Reference = named
        .parse()
        .map_err(|e| anyhow::anyhow!("--image: {named} is not an OCI reference: {e}"))?;
    let cache = Cache::default();
    let image = cache
        .find(&reference)?
        .ok_or_else(|| anyhow::anyhow!("--image: {reference} has not been pulled"))?;

    let at = cache
        .rootfs_path(&image.layers)
        .ok_or_else(|| anyhow::anyhow!("--image: {reference} has no layers to boot"))?;
    let rootfs = match Rootfs::open(&at) {
        Some(rootfs) => rootfs,
        None => {
            let layers: Vec<_> = image.layers.iter().map(|l| cache.layer_path(l)).collect();
            Rootfs::create(&layers, &at)?
        }
    };
    Ok((image, rootfs))
}

/// A listener on loopback for each published guest port, and a file saying which port each
/// one is at.
///
/// **The server's door is not this.** What a client was told to connect to is a listener the
/// server holds for the whole session, across every boot on it; this is the next hop, one per
/// boot, which the server joins each of its connections to. A port the system picks is what
/// lets two boots -- a `stop` and the next one, or two sessions side by side -- never ask
/// for the same one.
///
/// The file is written whole before the machine starts, and renamed into place, so a server
/// that finds it finds all of it. And it is there before the guest can say it is up, which is
/// the moment the server reads it: nothing has to be polled for.
///
/// One line to a door: the guest port, a space, and the port here.
fn relay(
    publish: &[u16],
    to: &std::path::Path,
) -> anyhow::Result<Vec<(u16, std::net::TcpListener)>> {
    use std::fmt::Write as _;

    let mut doors = Vec::with_capacity(publish.len());
    let mut said = String::new();
    for &console in publish {
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .map_err(|e| anyhow::anyhow!("--publish {console}: binding a relay: {e}"))?;
        let port = listener.local_addr()?.port();
        let _ = writeln!(said, "{console} {port}");
        doors.push((console, listener));
    }

    let partial = to.with_extension("partial");
    std::fs::write(&partial, said)
        .map_err(|e| anyhow::anyhow!("--relays: writing {}: {e}", partial.display()))?;
    std::fs::rename(&partial, to)
        .map_err(|e| anyhow::anyhow!("--relays: moving {} into place: {e}", to.display()))?;
    Ok(doors)
}

/// Whether venus has a Vulkan loader to reach on this host.
///
/// **Opened, not linked**, except on macOS: there the Vulkan is MoltenVK, built into this
/// binary, and there is nothing to find. Elsewhere virglrenderer opens the loader itself when
/// a GPU is made (Linux, `vulkan-dload`) or the linker defers it to the first call (Windows,
/// `/DELAYLOAD`) -- so a boot starts on a host without one, and this is the question asked
/// before either would find out. Opened the same way virglrenderer will, and let go again: a
/// boot that attaches a GPU opens it for real a moment later.
///
/// A loader is not a device -- a host can have one with no driver under it -- but that is
/// the loader's to report, as the device fails to come up, and not something a boot can
/// know without making an instance.
#[cfg(feature = "gpu")]
mod vulkan {
    #[cfg(target_os = "macos")]
    pub fn loader() -> anyhow::Result<()> {
        Ok(())
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    pub fn loader() -> anyhow::Result<()> {
        for name in [c"libvulkan.so.1", c"libvulkan.so"] {
            // SAFETY: a nul-terminated name; the handle is closed before it could be used.
            let handle = unsafe { libc::dlopen(name.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
            if !handle.is_null() {
                // SAFETY: the handle `dlopen` just returned, closed once.
                unsafe { libc::dlclose(handle) };
                return Ok(());
            }
        }
        anyhow::bail!(
            "this host has no Vulkan loader (libvulkan.so.1) for a GPU to be made with: \
             install it -- libvulkan1 on Debian and Ubuntu, vulkan-loader on Fedora -- along \
             with a Vulkan driver for the GPU"
        )
    }

    #[cfg(windows)]
    pub fn loader() -> anyhow::Result<()> {
        use windows_sys::Win32::System::LibraryLoader::LoadLibraryW;

        let name: Vec<u16> = "vulkan-1.dll\0".encode_utf16().collect();
        // SAFETY: a nul-terminated UTF-16 name that outlives the call. Left loaded: the
        // delay-loaded imports resolve against it on the first call a GPU makes.
        if unsafe { LoadLibraryW(name.as_ptr()) }.is_null() {
            anyhow::bail!(
                "this host has no Vulkan loader (vulkan-1.dll) for a GPU to be made with: it \
                 comes with the GPU's driver, or with the Vulkan Runtime from \
                 https://vulkan.lunarg.com"
            );
        }
        Ok(())
    }
}
