//! The machine from inside the process that becomes it: what [`enter`](super::enter) builds.
//!
//! # The devices, and what each is for
//!
//! ```text
//! virtio-fs  /dev/root      an empty root, holding the init and the directories it mounts
//!                           over -- replaced by the overlay before anything else runs
//! virtio-blk /dev/vda       the root filesystem's disk, read-only -- the overlay's lower
//! virtio-blk /dev/vdb       the snapshot -- the overlay's upper layer
//! virtio-con hvc0           the guest's console, into libkrun's log, which goes to ctrlout
//!            virtx-ctrl     the control channel to the agent, passed on to and from the host
//!            virtx-io       the I/O channel to the agent, the host's own pipes
//! virtio-gpu                Vulkan, on a machine that asked for it and a build that has it
//! virtio-vsock              the guest's network, on a machine with one or with published
//!                           ports -- see [`Network`]
//! virtio-fs  abin           virtx's own programs, read-only, at /abin -- when it was given some
//! virtio-fs  alib           what they run and load, read-only, at /alib -- when there is any
//! virtio-fs  mnt0, mnt1     one per share of a volume, mounted by tag after the pivot
//! virtio-fs  virtx          the agent, mounted at /.virtx after the pivot
//! ```
//!
//! **Attach order is what fixes the two disk names**, and the guest is told which is which
//! by name: the lower goes on first and is `/dev/vda`, the upper layer second and
//! `/dev/vdb`. The constants below and the order [`boot`] adds them in are one statement made
//! in two places, which is why they are next to each other.
//!
//! # Volumes
//!
//! A [`VolumeMount`] is one share for the volume's own files, and one more for each remote
//! store mounted in it, at its path under the volume's. They go on after both disks, since a
//! share takes no name from the order, and the init mounts each by its tag once the overlay
//! is the root -- in the order given, so a store's lands over the directory the volume's
//! own share holds for it. A store is served from this process, read-only.
//!
//! # The root that is thrown away
//!
//! The virtio-fs root is [`FsDevice::new_null`] -- a filesystem with nothing in it but what
//! the overlay puts there: the init binary, its config, and the empty directories the init
//! mounts the two disks over. It exists for the moment between the kernel handing over and
//! the init pivoting onto the overlay.
//!
//! # The agent
//!
//! What the init runs is not a command but `virtx-uvm-guest`, the agent, which runs the
//! commands the host sends. It is built into this binary (see `build.rs`) and shown on a
//! filesystem of its own -- a [`FsDevice::new_null`] with the agent in its overlay -- since one
//! on the root above would be left behind by the pivot. The init mounts it at [`AGENT_DIR`]
//! once the image is the root.
//!
//! # `/abin` and `/alib`
//!
//! [`MachineConfig::abin`], the programs a sandbox was made with, and the `alib/` directory beside
//! this executable, as `cargo xtask install` and a release put it there, each shared read-only
//! and mounted at [`ABIN_DIR`] and [`ALIB_DIR`]. `/abin` goes in front of the guest's `PATH`;
//! `/alib` is what is not a program to run by name, such as the X server `display` starts. A
//! machine with no `abin`, or a host with no `alib/`, boots without it rather than refusing to.
//! Both are mounted before the volumes, so a volume asked for at either lands over it.
//!
//! # The host
//!
//! Everything this process tells the host goes on `ctrlout`, a frame at a time ([`send`]):
//! libkrun's log -- the guest's console among it -- and whatever the agent says on the
//! [`PORT_NAME`] port, `Ready` with where the ports are published filled in. What the host
//! sends on `ctrlin` goes on to the agent, and `ctrlin` closing shuts the guest down and ends the
//! process. `ioin` and `ioout` are the [`IO_PORT_NAME`] port's own ends, which nothing here reads
//! or writes.

use std::convert::Infallible;
use std::fs::File;
use std::io;
use std::net::{Ipv4Addr, TcpListener};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

use krun::{
    BalloonDevice, BlockDevice, ConsoleDevice, DiskFormat, FreePageReportingMode, FsDevice,
    FsOverlay, MmioDeviceManager, Payload, RngDevice, VmmBuilder,
};
use oci_spec::image::Config;
use serde::{Deserialize, Serialize};

#[cfg(feature = "gpu")]
use super::gpu;
use super::wire::{self, CtrlReq, CtrlResp, IO_PORT_NAME, PORT_NAME};
use super::{Error, MachineConfig, Network, Result, STOP_TIMEOUT, net};
use crate::fstree::FsTree;
use crate::snapshot::Snapshot;
use crate::volume::{Remote, Served};

/// The tag the guest kernel mounts as `/`, which libkrun's default kernel cmdline names
/// with `rootfstype=virtiofs`.
const ROOT_TAG: &str = "/dev/root";

/// Where the two disks land in the guest, in the order [`boot`] attaches them.
const LOWER_DEVICE: &str = "/dev/vda";
const UPPER_DEVICE: &str = "/dev/vdb";

/// What each is, which the guest is told rather than left to probe.
const LOWER_FSTYPE: &str = "erofs";
const UPPER_FSTYPE: &str = "ext4";

/// The directories the init needs on the root before it can mount anything, since a
/// filesystem with nothing in it has nowhere to mount over.
const ROOT_DIRS: [&str; 6] = ["dev", "proc", "sys", "newroot", "lower", "upper"];

/// What each share is attached under, with its place in the list after it. A tag is only a
/// name the two ends agree on, and one nothing else uses is all this needs.
const MOUNT_TAG: &str = "mnt";

/// What the guest mounts a share as, which is the only thing a virtio-fs share can be.
const MOUNT_FSTYPE: &str = "virtiofs";

/// `S_IFDIR | 0755`, as [`FsOverlay::add_dir`] wants a mode.
const DIR_MODE: u32 = 0o040_755;

/// What a `PATH`-less image gets, which is what docker gives one.
const DEFAULT_PATH: &str = "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// The agent, as `build.rs` built it for the guest.
const AGENT: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/virtx-uvm-guest"));

/// Where the agent's filesystem is mounted in the guest, the tag it is attached under, and the
/// agent's name in it.
const AGENT_DIR: &str = "/.virtx";
const AGENT_TAG: &str = "virtx";
const AGENT_NAME: &str = "virtx-uvm-guest";

/// Where virtx's own programs are mounted in the guest, and the tag they are attached under.
const ABIN_DIR: &str = "/abin";
const ABIN_TAG: &str = "abin";
/// The same for what those programs run and load, and the directory beside this executable it
/// is shared from.
const ALIB_DIR: &str = "/alib";
const ALIB_TAG: &str = "alib";
const ALIB_HOST: &str = "alib";

/// The log target libkrun gives what the guest's console prints.
const CONSOLE_TARGET: &str = "init_or_kernel";

/// What a machine's process is spawned with, as one argument: the machine, and its ends of
/// the control channel and the I/O channel.
#[derive(Serialize, Deserialize)]
pub(super) struct BootArgs {
    pub config: MachineConfig,
    pub ctrlin: u64,
    pub ctrlout: u64,
    pub ioin: u64,
    pub ioout: u64,
}

/// `ctrlout`, once this process [holds](hold) it.
static CTRLOUT: OnceLock<Mutex<File>> = OnceLock::new();

/// Build `machine`, running the agent, and become it: load the guest kernel, lay the init over
/// an empty root, attach the devices, pass `ctrlin` on to the agent and the agent on to
/// `ctrlout`, give it `io`, `ioin` and `ioout`, as its I/O port, and run.
///
/// **This does not return.** The VMM exits this process with the guest's own status, so what
/// comes back is a machine that could not be built, or a failure of the event loop.
///
/// On macOS the calling binary needs the `com.apple.security.hypervisor` entitlement.
pub(super) fn boot(machine: MachineConfig, ctrlin: File, io: (File, File)) -> Result<Infallible> {
    // Before anything is built, so a machine over a snapshot another one holds is refused
    // with nothing to undo -- unless whoever started it holds the lock for it.
    let lock = match machine.snapshot_held {
        true => None,
        false => machine.snapshot.as_ref().map(Snapshot::lock).transpose()?,
    };

    // What PID 1 does once the guest is up: run the agent, mounted with its filesystem below,
    // and end the machine when it does. The init binary and this config are injected into the
    // root through the overlay
    // below, and `apply` is what appends `init=/init.krun` to the kernel cmdline. The two
    // devices go in as environment because that is how libkrun's init is told where its root
    // is.
    let abin = machine.abin.clone();
    #[cfg(windows)]
    if let Some(dir) = &abin {
        executable(dir);
    }
    let alib = beside(ALIB_HOST);
    let env = env(&machine.config, abin.is_some());
    let mut init = krun_init::Config::builder()
        .args(&[&format!("{AGENT_DIR}/{AGENT_NAME}")])
        .env(&env.iter().map(String::as_str).collect::<Vec<_>>())
        .workdir(machine.config.working_dir().as_deref().unwrap_or("/"))
        .env_var(&format!("KRUN_BLOCK_LOWER_DEVICE={LOWER_DEVICE}"))
        .env_var(&format!("KRUN_BLOCK_LOWER_FSTYPE={LOWER_FSTYPE}"))
        .env_var(&format!("KRUN_BLOCK_UPPER_DEVICE={UPPER_DEVICE}"))
        .env_var(&format!("KRUN_BLOCK_UPPER_FSTYPE={UPPER_FSTYPE}"));
    if machine.spec.network != Network::None {
        init = init.env_var(&format!("{}={}", net::NAMESERVER_ENV, net::NAMESERVER));
    }

    // The virtio-fs devices the guest is shown, which the init mounts by their tags in this
    // order: each volume's files and the stores under them, with each volume held. Refused
    // where one could not be shown where it was asked.
    let mut shares = Vec::new();
    for (dir, at, tag) in [(&abin, ABIN_DIR, ABIN_TAG), (&alib, ALIB_DIR, ALIB_TAG)] {
        let Some(dir) = dir else { continue };
        let host = utf8(dir)?;
        init = init.mount(at, MOUNT_FSTYPE, tag);
        shares.push(
            FsDevice::new_read_only(tag, host)
                .map_err(|e| Error::vmm(format!("sharing {host} as {tag}"), e))?,
        );
    }
    let mut holds = Vec::with_capacity(machine.volumes.len());
    for (i, asked) in machine.volumes.iter().enumerate() {
        let volume = crate::Volume::open(&asked.path)?;
        let refused = |why| {
            Err(Error::MountPoint {
                volume: volume.name().to_owned(),
                at: asked.at.clone(),
                why,
            })
        };
        // Asked of the text rather than of `Path`, which answers by this host's rules: on
        // Windows `/data` is not absolute.
        let at = asked.at.trim_end_matches('/');
        if !asked.at.starts_with('/') {
            return refused("a path in the guest is absolute");
        }
        if at.is_empty() {
            return refused("/ is the guest's root");
        }
        if machine.volumes[..i]
            .iter()
            .any(|other| other.at.trim_end_matches('/') == at)
        {
            return refused("another volume is mounted there");
        }

        holds.push(volume.hold()?);
        let tag = format!("{MOUNT_TAG}{}", shares.len());
        let files = volume.files();
        let host = utf8(&files)?;
        init = init.mount(at, MOUNT_FSTYPE, &tag);
        // Read-only at the device, which is the whole of what `read_only` promises.
        shares.push(
            match asked.read_only {
                true => FsDevice::new_read_only(&tag, host),
                false => FsDevice::new(&tag, host),
            }
            .map_err(|e| Error::vmm(format!("sharing {host} as {tag}"), e))?,
        );
        for mount in volume.mounts() {
            let served =
                Served::new(Arc::new(Remote::new(&mount.source)?)).map_err(|e| Error::Io {
                    context: format!("serving the mount at {} in {}", mount.path, volume.name()),
                    source: e,
                })?;
            let tag = format!("{MOUNT_TAG}{}", shares.len());
            init = init.mount(&format!("{at}/{}", mount.path), MOUNT_FSTYPE, &tag);
            shares.push(
                FsDevice::new_external(&tag, Arc::new(served))
                    .map_err(|e| Error::vmm(format!("serving {tag}"), e))?,
            );
        }
    }

    let mut agent = FsOverlay::new();
    agent
        .add_file(AGENT_NAME, AGENT, 0o755, false)
        .map_err(|e| Error::vmm("putting the agent on its filesystem", e))?;
    let mut agent_fs = FsDevice::new_null(AGENT_TAG)
        .map_err(|e| Error::vmm("making the agent's filesystem", e))?;
    agent_fs.set_overlay(agent);
    init = init.mount(AGENT_DIR, MOUNT_FSTYPE, AGENT_TAG);

    // Leaked, because the root borrows it for as long as the machine runs, which is the rest
    // of the process.
    let init: &'static krun_init::Config = Box::leak(Box::new(init.build()));

    let published = published(&machine.ports)?;
    let addresses: Vec<_> = published
        .iter()
        .map(|(port, listener)| Ok((*port, listener.local_addr()?)))
        .collect::<io::Result<_>>()
        .map_err(|e| Error::io("reading where the published ports are", e))?;

    let upper = match &machine.snapshot {
        Some(snapshot) => snapshot.path.clone(),
        None => {
            let path =
                std::env::temp_dir().join(format!("virtx-uvm-upper-{}.ext4", std::process::id()));
            let file = File::create(&path).map_err(|e| Error::Io {
                context: format!("creating {}", path.display()),
                source: e,
            })?;
            FsTree::new().to_ext4(file, Snapshot::SIZE)?;
            path
        }
    };

    // The guest kernel, which libkrun `dlopen`s rather than links.
    let mut payload =
        Payload::load_krunfw().map_err(|e| Error::vmm("loading the guest kernel", e))?;

    let mut overlay = FsOverlay::new();
    for dir in ROOT_DIRS {
        overlay
            .add_dir(dir, DIR_MODE)
            .map_err(|e| Error::vmm(format!("making /{dir} on the boot root"), e))?;
    }
    init.apply(&mut overlay, &mut payload)
        .map_err(Error::Init)?;
    // Under nested virtualization -- an EC2 node -- each check the guest kernel makes of its
    // clock, and each PCI slot it probes, is a trip to the outer hypervisor: some 70 ms of a
    // boot there. Its TSC is the host's, invariant; and a machine without a GPU has no PCI
    // device, its devices being virtio-mmio's. x86_64's own: elsewhere the kernel would not
    // know them, and would pass them to the init as its environment.
    #[cfg(target_arch = "x86_64")]
    {
        payload.append_cmdline("tsc=reliable no_timer_check");
        if !machine.spec.gpu {
            payload.append_cmdline("pci=off");
        }
    }

    let mut root =
        FsDevice::new_null(ROOT_TAG).map_err(|e| Error::vmm("making the boot root", e))?;
    root.set_overlay(overlay);

    // The guest's console goes to libkrun's log, and the agent's port to two pipes: one the
    // host's messages are written into for the agent to read, one it writes into for this
    // process to pass on. The I/O port goes to the host's own two. Their port ends leaked,
    // because the console borrows them for the rest of the process.
    let (to_guest, mut for_guest) =
        io::pipe().map_err(|e| Error::io("making the agent's input", e))?;
    let (mut from_guest, of_guest) =
        io::pipe().map_err(|e| Error::io("making the agent's output", e))?;
    let to_guest: &'static io::PipeReader = Box::leak(Box::new(to_guest));
    let of_guest: &'static io::PipeWriter = Box::leak(Box::new(of_guest));
    let (ioin, ioout): (&'static File, &'static File) =
        (Box::leak(Box::new(io.0)), Box::leak(Box::new(io.1)));
    #[cfg(unix)]
    let (to_guest, of_guest, ioin, ioout) = {
        use std::os::fd::AsFd;
        (
            to_guest.as_fd(),
            of_guest.as_fd(),
            ioin.as_fd(),
            ioout.as_fd(),
        )
    };
    #[cfg(windows)]
    let (to_guest, of_guest, ioin, ioout) = {
        use std::os::windows::io::AsHandle;
        (
            to_guest.as_handle(),
            of_guest.as_handle(),
            ioin.as_handle(),
            ioout.as_handle(),
        )
    };
    let mut console = ConsoleDevice::builder();
    console
        .add_default_console(None, None, None)
        .map_err(|e| Error::vmm("making the guest console", e))?;
    console
        .add_inout_port(PORT_NAME, Some(to_guest), Some(of_guest))
        .map_err(|e| Error::vmm("wiring the agent's port", e))?;
    console
        .add_inout_port(IO_PORT_NAME, Some(ioin), Some(ioout))
        .map_err(|e| Error::vmm("wiring the agent's I/O port", e))?;

    // Read-only at the device and not merely in the guest's mount flags: what is behind
    // it is the layer files, shared with every other root filesystem over them.
    let rootfs = utf8(&machine.rootfs)?;
    let mut lower = BlockDevice::new("vda", rootfs, DiskFormat::Vmdk)
        .map_err(|e| Error::vmm(format!("attaching {rootfs}"), e))?;
    lower.set_read_only(true);

    let upper_path = utf8(&upper)?;
    // A snapshot with a base: read and written through it, what is written told apart for
    // its next save; and, lazy, its chunks fetched over the host's socket as they are first
    // read, and the rest in the background until it is whole.
    #[cfg(unix)]
    let lazy = match &machine.snapshot {
        Some(_) if crate::lazy::has_base(&upper) => {
            let source: Arc<dyn crate::lazy::ChunkSource> = match &machine.chunks {
                Some(socket) => Arc::new(crate::lazy::SocketSource::new(socket.clone())),
                None => Arc::new(crate::lazy::NoChunks),
            };
            Some(
                crate::lazy::LazyDisk::open(&upper, source)
                    .map_err(|e| Error::io(format!("opening {upper_path} by its base"), e))?,
            )
        }
        // One with no base, new: its boot set noted, for its first lazy boot elsewhere.
        Some(_) => Some(
            crate::lazy::LazyDisk::open_unbased(&upper)
                .map_err(|e| Error::io(format!("opening {upper_path}"), e))?,
        ),
        _ => None,
    };
    #[cfg(not(unix))]
    let lazy: Option<std::convert::Infallible> = None;
    let upper_device = match &lazy {
        #[cfg(unix)]
        Some(disk) => {
            BlockDevice::with_storage("vdb", upper_path, DiskFormat::Raw, Box::new(disk.clone()))
        }
        #[cfg(not(unix))]
        Some(never) => match *never {},
        None => BlockDevice::new("vdb", upper_path, DiskFormat::Raw),
    }
    .map_err(|e| Error::vmm(format!("attaching {upper_path}"), e))?;
    // Told its machine has booted, as its agent says it is ready: its boot set ends there.
    #[cfg(unix)]
    let booting = lazy.clone();
    // Marked and rebased as the host keeps it while it runs.
    #[cfg(unix)]
    let kept = lazy.clone();
    #[cfg(unix)]
    if let Some(disk) = lazy.filter(|disk| !disk.whole()) {
        std::thread::spawn(move || fill(disk));
    }

    let mut devices = MmioDeviceManager::new();
    devices.add(root);
    devices.add(
        console
            .build()
            .map_err(|e| Error::vmm("building the guest console", e))?,
    );
    devices.add(RngDevice::new().map_err(|e| Error::vmm("attaching an entropy source", e))?);
    let mut balloon = BalloonDevice::new().map_err(|e| Error::vmm("attaching a balloon", e))?;
    // macOS gives back reported pages only by replacing them; elsewhere this does nothing.
    balloon.set_free_page_reporting_mode(FreePageReportingMode::Replace);
    devices.add(balloon);

    #[cfg(feature = "gpu")]
    if machine.spec.gpu {
        devices.add(gpu::device(machine.spec.gpu_memory_mib)?);
    }

    // The whole of what having neither is: no device -- see `net`. And so no port published
    // later either: a guest with no device has nothing a listen could be carried over.
    let publisher = if machine.spec.network != Network::None || !published.is_empty() {
        let (device, publisher) = net::attach(machine.spec.network, published)?;
        devices.add(device);
        Some(publisher)
    } else {
        None
    };

    // In this order, and the order is the whole of what names them -- see the module
    // doc.
    devices.add(lower);
    devices.add(upper_device);

    for share in shares {
        devices.add(share);
    }
    devices.add(agent_fs);

    let vmm = VmmBuilder::new()
        .vcpus(machine.spec.vcpus)
        .map_err(|e| Error::vmm(format!("{} vCPUs", machine.spec.vcpus), e))?
        .ram_mib(machine.spec.ram_mib)
        .map_err(|e| Error::vmm(format!("{} MiB of memory", machine.spec.ram_mib), e))?
        .payload(payload)
        .devices(devices)
        .build()
        .map_err(|e| Error::vmm("building the machine", e))?;

    // The disk is open now, so a temporary one can lose its name and go with the
    // process. Windows will not remove an open file, and leaves it in the temp directory.
    #[cfg(unix)]
    if machine.snapshot.is_none() {
        let _ = std::fs::remove_file(&upper);
    }

    // The host's messages on to the agent, and the agent's back -- `Ready` with where the ports
    // are published, which only this side knows. A host that has closed `ctrlin`, or written
    // something unreadable into it, is gone and will hear nothing more: the guest is shut down
    // as the host would, killed if it has not ended by `STOP_TIMEOUT`. An agent that has
    // stopped, or said something unreadable, takes no more commands, and the machine ends so
    // the host sees it has.
    let mut ctrlin = ctrlin;
    std::thread::spawn(move || {
        while let Ok(Some(message)) = wire::recv::<CtrlReq>(&mut ctrlin) {
            match message {
                CtrlReq::Mark { mark } => {
                    #[cfg(unix)]
                    let marked = match &kept {
                        Some(disk) => disk.mark(mark).map(|()| true).map_err(|e| e.to_string()),
                        None => Ok(false),
                    };
                    #[cfg(not(unix))]
                    let marked = Ok(false);
                    let _ = send(&CtrlResp::Marked { mark, marked });
                    continue;
                }
                CtrlReq::Rebase { mark } => {
                    #[cfg(unix)]
                    let rebased = match &kept {
                        Some(disk) => disk.rebase(mark).map_err(|e| e.to_string()),
                        None => Ok(false),
                    };
                    #[cfg(not(unix))]
                    let rebased = Ok(false);
                    let _ = send(&CtrlResp::Rebased { mark, rebased });
                    continue;
                }
                _ => {}
            }
            if let CtrlReq::Publish { port } = message {
                let published = match &publisher {
                    Some(publisher) => publisher.publish(port),
                    None => Err(format!(
                        "guest port {port} cannot be published: the machine was started with no network and no ports"
                    )),
                };
                let _ = send(&CtrlResp::Published(published));
                continue;
            }
            let _ = wire::send(&mut for_guest, &message);
        }
        let _ = wire::send(&mut for_guest, &CtrlReq::Shutdown);
        std::thread::sleep(STOP_TIMEOUT);
        std::process::exit(1);
    });
    std::thread::spawn(move || {
        while let Ok(Some(message)) = wire::recv::<CtrlResp>(&mut from_guest) {
            let _ = send(&match message {
                CtrlResp::Ready(_) => {
                    #[cfg(unix)]
                    if let Some(disk) = &booting {
                        disk.booted();
                    }
                    CtrlResp::Ready(addresses.clone())
                }
                message => message,
            });
        }
        std::process::exit(1);
    });

    // Held for as long as the machine runs -- which is the rest of the process, whose end is
    // what lets them go.
    let _held = (lock, holds);
    vmm.run();
    Err(Error::EventLoop)
}

/// Take the ends the host left this process: from here on everything it says goes to
/// `ctrlout`, libkrun's log among it. Answers `ctrlin`, for [`boot`] to pass on to the agent.
///
/// # Safety
///
/// `ctrlin` and `ctrlout` are ends the host left open for this process, which nothing else in it
/// holds.
pub(super) unsafe fn hold(ctrlin: u64, ctrlout: u64) -> File {
    // SAFETY: the caller's.
    let (ctrlin, ctrlout) = unsafe { (from_raw(ctrlin), from_raw(ctrlout)) };
    let _ = CTRLOUT.set(Mutex::new(ctrlout));
    if log::set_logger(&Logger).is_ok() {
        // Everything with `VIRTX_MACHINE_LOG` set, as its host says everything then too.
        log::set_max_level(match std::env::var_os("VIRTX_MACHINE_LOG") {
            Some(_) => log::LevelFilter::Debug,
            None => log::LevelFilter::Warn,
        });
    }
    ctrlin
}

/// Tell the host `message`.
pub(super) fn send(message: &CtrlResp) -> io::Result<()> {
    let ctrlout = CTRLOUT
        .get()
        .ok_or_else(|| io::Error::other("ctrlout is not held"))?;
    wire::send(&mut *ctrlout.lock().unwrap(), message)
}

/// A pipe end the host left this process, as it named it.
///
/// # Safety
///
/// `end` is an end the host left open for this process, which nothing else in it holds.
#[cfg(unix)]
pub(super) unsafe fn from_raw(end: u64) -> File {
    // SAFETY: the caller's.
    unsafe { std::os::fd::FromRawFd::from_raw_fd(end as i32) }
}

#[cfg(windows)]
pub(super) unsafe fn from_raw(end: u64) -> File {
    // SAFETY: the caller's.
    unsafe { std::os::windows::io::FromRawHandle::from_raw_handle(end as usize as _) }
}

/// The machine process's `log`, each record a [`Log`](CtrlResp::Log) on `ctrlout` as it is
/// made: in the thread that makes it, so what the console's last lines are is out before
/// libkrun ends the process.
///
/// Whatever logs through the `log` facade in this process comes here -- not libkrun alone:
/// libkrun's records, the guest's console among them (under [`CONSOLE_TARGET`]), and this
/// crate's own, such as why the machine could not be built.
struct Logger;

impl log::Log for Logger {
    fn enabled(&self, _: &log::Metadata) -> bool {
        true
    }

    fn log(&self, record: &log::Record) {
        let message = format!("{}: {}", record.target(), record.args());
        // A terminal ends its lines with `\r\n`, and libkrun takes off only the `\n`.
        let message = match record.target() {
            CONSOLE_TARGET => message.trim_end_matches('\r').to_string(),
            _ => message,
        };
        let _ = send(&CtrlResp::Log {
            level: record.level().to_string(),
            message,
        });
    }

    fn flush(&self) {}
}
/// A listener on this host's loopback for each of `ports`, which the guest's listener on that
/// port is relayed from. Refused for `0`, which is no port, and for one given twice.
fn published(ports: &[u16]) -> Result<Vec<(u16, TcpListener)>> {
    let mut published = Vec::with_capacity(ports.len());
    for (i, &port) in ports.iter().enumerate() {
        let refused = |why| Err(Error::Port { port, why });
        if port == 0 {
            return refused("not a port");
        }
        if ports[..i].contains(&port) {
            return refused("given twice");
        }
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).map_err(|e| Error::Io {
            context: format!("binding a relay for guest port {port}"),
            source: e,
        })?;
        published.push((port, listener));
    }
    Ok(published)
}

/// A path as libkrun takes it, which is a `&str`. Refused rather than lossily converted: a
/// name with a replacement character in it opens some other file or none.
fn utf8(path: &Path) -> Result<&str> {
    path.to_str().ok_or_else(|| Error::Io {
        context: format!(
            "{} is not valid UTF-8, which libkrun cannot name",
            path.display()
        ),
        source: std::io::Error::from(std::io::ErrorKind::InvalidInput),
    })
}

/// The directory `name` beside this executable, such as `alib/`, if there is one, with its
/// programs made executable in the guest.
fn beside(name: &str) -> Option<std::path::PathBuf> {
    let dir = std::env::current_exe().ok()?.parent()?.join(name);
    if !dir.is_dir() {
        return None;
    }
    #[cfg(windows)]
    executable(&dir);
    Some(dir)
}

/// Mark each file directly in `dir` executable for the guest: the programs `/abin` and `/alib`
/// run by path. What is in their directories is read, not run -- a library is mapped by the
/// guest's loader whatever its mode, and `xkb/` is data -- so it is left as it is, and a boot
/// writes a stream for a handful of files rather than for every keymap.
///
/// libkrun's virtiofs on Windows answers `0644` for a regular file unless an alternate data
/// stream beside it, in libkrun's own `uid:gid:0mode` form, says otherwise -- and NTFS keeps no
/// mode for an unpacked release to have brought. Every boot, so a directory replaced since is
/// marked again. A file it fails on is said and skipped, as a host with no `alib/` is a machine
/// without it.
#[cfg(windows)]
fn executable(dir: &Path) {
    const OVERRIDE_STAT: &str = ":user.containers.override_stat";
    const ROOT_EXECUTABLE: &str = "0:0:0100755";

    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for path in entries.flatten().map(|entry| entry.path()) {
        if !path.is_file() {
            continue;
        }
        let mut stream = path.clone().into_os_string();
        stream.push(OVERRIDE_STAT);
        if let Err(e) = std::fs::write(std::path::PathBuf::from(stream), ROOT_EXECUTABLE) {
            log::warn!(
                "{} will not be executable in the guest: {e}",
                path.display()
            );
        }
    }
}

/// The environment the guest runs in: the image's, with `PATH` and `HOME` added when it
/// sets neither, with the values docker uses, and [`ABIN_DIR`] in front of `PATH` when `abin`
/// says the machine has one.
///
/// In front rather than behind: `/abin` is what virtx undertakes to provide, and an image's own
/// `index` should not be what a caller reading `abin/skills/` ends up running.
fn env(config: &Config, abin: bool) -> Vec<String> {
    let mut env = config.env().clone().unwrap_or_default();
    let set = |env: &[String], name: &str| env.iter().any(|var| var.starts_with(name));
    if !set(&env, "PATH=") {
        env.push(DEFAULT_PATH.to_string());
    }
    if !set(&env, "HOME=") {
        env.push("HOME=/root".to_string());
    }
    if abin && let Some(path) = env.iter_mut().find(|var| var.starts_with("PATH=")) {
        *path = match &path["PATH=".len()..] {
            "" => format!("PATH={ABIN_DIR}"),
            rest => format!("PATH={ABIN_DIR}:{rest}"),
        };
    }
    env
}

/// `disk` fetched whole, in the background of its machine: tried again a while after a chunk
/// could not be, until it is.
#[cfg(unix)]
fn fill(disk: crate::lazy::LazyDisk) {
    // What its last boot touched first, ahead of the guest asking for it; then the rest.
    disk.fetch_boot_set(crate::lazy::BOOT_AT_ONCE);
    while let Err(e) = disk.fill(crate::lazy::FILL_AT_ONCE) {
        log::warn!("filling {disk}: {e}; again in 5 s");
        std::thread::sleep(std::time::Duration::from_secs(5));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_the_image_did_not_set_is_added_and_one_it_did_is_left_alone() {
        let mut stated = Config::default();
        stated.set_env(Some(vec!["PATH=/opt/bin".to_string()]));
        let stated = env(&stated, false);
        assert!(stated.contains(&"PATH=/opt/bin".to_string()), "{stated:?}");
        assert!(!stated.iter().any(|var| var == DEFAULT_PATH), "{stated:?}");

        let silent = env(&Config::default(), false);
        assert!(silent.iter().any(|var| var == DEFAULT_PATH), "{silent:?}");
        assert!(silent.contains(&"HOME=/root".to_string()), "{silent:?}");
    }

    #[test]
    fn abin_goes_in_front_of_the_path_only_when_the_machine_has_one() {
        let mut stated = Config::default();
        stated.set_env(Some(vec!["PATH=/opt/bin".to_string()]));
        assert!(env(&stated, true).contains(&"PATH=/abin:/opt/bin".to_string()));

        let silent = env(&Config::default(), true);
        let path = format!("PATH=/abin:{}", &DEFAULT_PATH["PATH=".len()..]);
        assert!(silent.contains(&path), "{silent:?}");
        assert_eq!(
            silent.iter().filter(|var| var.starts_with("PATH=")).count(),
            1
        );

        let mut empty = Config::default();
        empty.set_env(Some(vec!["PATH=".to_string()]));
        assert!(env(&empty, true).contains(&"PATH=/abin".to_string()));

        assert!(
            !env(&Config::default(), false)
                .iter()
                .any(|var| var.contains("/abin"))
        );
    }

    #[test]
    fn a_port_is_refused_where_it_cannot_be_published() {
        assert!(matches!(published(&[0]), Err(Error::Port { port: 0, .. })));
        assert!(matches!(
            published(&[80, 443, 80]),
            Err(Error::Port { port: 80, .. })
        ));
        let ports: Vec<_> = published(&[80, 443])
            .unwrap()
            .iter()
            .map(|(port, listener)| (*port, listener.local_addr().unwrap().ip()))
            .collect();
        assert_eq!(
            ports,
            [
                (80, Ipv4Addr::LOCALHOST.into()),
                (443, Ipv4Addr::LOCALHOST.into())
            ]
        );
    }
}
