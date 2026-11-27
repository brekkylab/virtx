//! The machine from inside the process that becomes it: what [`enter`](super::enter) builds.
//!
//! # The devices, and what each is for
//!
//! ```text
//! virtio-fs  /dev/root      the boot root, holding the init, the agent and the directories
//!                           the agent mounts over -- left for the overlay before any
//!                           command runs
//! virtio-blk /dev/vda       the root filesystem's disk, read-only -- the overlay's lower
//! virtio-blk /dev/vdb       the upper disk -- the overlay's upper layer
//! virtio-con hvc0           the guest's console, into libkrun's log, which goes to ctrlout
//!            virtx-ctrl     the control channel to the agent, passed on to and from the host
//!            virtx-io       the I/O channel to the agent, the host's own pipes
//! virtio-gpu                Vulkan, on a machine that asked for it and a build that has it
//! virtio-net eth0           the guest's network, on a machine with one, with published ports
//!                           or with its client's -- see `net`
//! virtio-fs  abin           virtx's own programs, read-only, at /abin -- when it was given some
//! virtio-fs  alib           what they run and load, read-only, at /alib -- when there is any
//! virtio-fs  mnt0, mnt1     one per share of a volume, mounted by tag once the overlay is
//!                           the root
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
//! share takes no name from the order, and the agent mounts each by its tag once the overlay
//! is the root -- in the order given, so a store's lands over the directory the volume's
//! own share holds for it. A store is served from this process, read-only.
//!
//! # The boot root
//!
//! The virtio-fs root is [`FsDevice::new_null`] -- a filesystem with nothing in it but what
//! the overlay puts there: the init binary, its config, the agent, and the empty directories
//! the agent mounts the two disks over. libkrun's init stays on it as PID 1, which is where it
//! tells the host how the machine ended; everything else leaves it for the overlay.
//!
//! # The agent
//!
//! What the init runs is not a command but `virtx-uvm-guest`, the agent, built into this
//! binary (see `build.rs`). The first thing it reads on its port is a
//! [`MachineInfo`](wire::MachineInfo), which this process writes there: it builds the root
//! that names -- the upper disk overlaid on the lower, the shares mounted, the network set up
//! -- steps into it, and runs the commands the host sends.
//!
//! # `/abin` and `/alib`
//!
//! [`MachineConfig::abin`], the programs a sandbox was made with, and the `alib/` directory beside
//! this executable, as `cargo xtask install` and a release put it there, each shared read-only
//! and mounted at [`ABIN_DIR`] and [`ALIB_DIR`]. `/abin` goes in front of the guest's `PATH`;
//! `/alib` is the libraries a GPU sandbox's programs load. A machine with no `abin`, or a host
//! with no `alib/`, boots without it rather than refusing to.
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
use std::time::{Duration, Instant};

use krun::{
    BalloonDevice, BlockDevice, ConsoleDevice, DiskFormat, FreePageReportingMode, FsDevice,
    FsOverlay, MmioDeviceManager, Payload, RngDevice, VmmBuilder,
};
use oci_spec::image::Config;
use serde::{Deserialize, Serialize};

#[cfg(feature = "gpu")]
use super::gpu;
use super::wire::GuestNetwork;
use super::wire::{self, CtrlReq, CtrlResp, IO_PORT_NAME, PORT_NAME};
use super::{Error, MachineConfig, Result, STOP_TIMEOUT, net};
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

/// The directories the init and the agent need on the boot root before they can mount
/// anything, since a filesystem with nothing in it has nowhere to mount over.
const ROOT_DIRS: [&str; 6] = ["dev", "proc", "sys", "newroot", "lower", "upper"];

/// What each share is attached under, with its place in the list after it. A tag is only a
/// name the two ends agree on, and one nothing else uses is all this needs.
const MOUNT_TAG: &str = "mnt";

/// `S_IFDIR | 0755`, as [`FsOverlay::add_dir`] wants a mode.
const DIR_MODE: u32 = 0o040_755;

/// What a `PATH`-less image gets, which is what docker gives one.
const DEFAULT_PATH: &str = "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// The agent, as `build.rs` built it for the guest.
const AGENT: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/virtx-uvm-guest"));

/// The agent's name on the boot root.
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
pub(super) const CONSOLE_TARGET: &str = "init_or_kernel";

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

/// Whether a machine's RAM is backed with host pages before it boots rather than as the guest
/// first touches them: `VIRTX_PREFAULT_MEMORY=1`, off otherwise.
///
/// Only WHP heeds it. There each first touch is a fault the hypervisor resolves on its own,
/// and nested -- a Windows instance in a cloud, as a CI runner is -- one costs about 130µs:
/// the guest's struct pages alone, initialised before it reaches init, are 2s of a 3.8s
/// boot, where faulting all 4 GiB in at once is 1.2s, and the machine tests there take 59s
/// rather than 182. On a desktop the faults are cheap and the cost is all the other way:
/// every machine holds its whole RAM on the host from the start, used or not. So it is for a
/// runner to turn on, and nothing else.
fn prefault_memory() -> bool {
    std::env::var_os("VIRTX_PREFAULT_MEMORY").is_some_and(|v| v == "1")
}

/// How often the agent is [pinged](CtrlReq::Ping) once it is up.
const PING_EVERY: Duration = Duration::from_secs(5);

/// How long past its second the silence check may wake before this process is taken to have
/// been paused meanwhile ([`Machine::pause`](super::Machine::pause)) rather than slow. It cannot
/// tell a pause from a stall of the process as long, as on a host loaded enough to leave it
/// unscheduled: a stall is forgiven as a pause is, the silence started anew.
const PAUSED_PAST: Duration = Duration::from_secs(3);

/// How long the agent may say nothing, pings unanswered, before its guest is taken to have
/// stopped and the machine is ended: `VIRTX_GUEST_SILENCE`, in seconds, `0` for never, or a
/// minute, and no less than two pings apart. A guest that halts, as `halt -f` does or a
/// power-off without ACPI, leaves its vCPUs idle and its process running, and nothing else ends
/// it. A busy guest answers within moments; a minute is as long as the host waits on a guest's
/// files ([`IO_TIMEOUT`](super::IO_TIMEOUT)).
fn guest_silence() -> Duration {
    match std::env::var("VIRTX_GUEST_SILENCE").map(|seconds| seconds.parse::<u64>()) {
        // No time elapsed is past it.
        Ok(Ok(0)) => Duration::MAX,
        Ok(Ok(seconds)) => Duration::from_secs(seconds).max(2 * PING_EVERY),
        _ => Duration::from_secs(60),
    }
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
    let abin = machine.abin.clone();
    #[cfg(windows)]
    if let Some(dir) = &abin {
        executable(dir, true);
    }
    let alib = beside(ALIB_HOST);
    let env = env(&machine.config, abin.is_some());
    let mut info = wire::MachineInfo {
        lower_device: LOWER_DEVICE.to_owned(),
        lower_fstype: LOWER_FSTYPE.to_owned(),
        upper_device: UPPER_DEVICE.to_owned(),
        upper_fstype: UPPER_FSTYPE.to_owned(),
        shares: Vec::new(),
        network: (machine.spec.network
            || !machine.ports.is_empty()
            || machine.client_ports.is_some())
        .then(|| GuestNetwork::new(machine.spec.network)),
        workdir: workdir(&machine.config),
        seed: host_seed(),
        now: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .map(|since| (since.as_secs() as i64, since.subsec_nanos())),
    };

    // The virtio-fs devices the guest is shown, which the agent mounts by their tags in this
    // order: each volume's files and the stores under them, with each volume held. Refused
    // where one could not be shown where it was asked.
    let mut shares = Vec::new();
    for (dir, at, tag) in [(&abin, ABIN_DIR, ABIN_TAG), (&alib, ALIB_DIR, ALIB_TAG)] {
        let Some(dir) = dir else { continue };
        let host = utf8(dir, at)?;
        info.shares.push((tag.to_owned(), at.to_owned()));
        shares.push(
            FsDevice::new_read_only(tag, host)
                .map_err(|e| Error::vmm(format!("sharing {at}"), e))?,
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
        // The guest is given the spelling checked, not the one asked.
        let at = match crate::mount_point(&asked.at) {
            Ok(at) => at,
            Err(why) => return refused(why),
        };
        if machine.volumes[..i]
            .iter()
            .any(|other| crate::mount_point(&other.at).is_ok_and(|other| other == at))
        {
            return refused("another volume is mounted there");
        }

        holds.push(volume.hold()?);
        let tag = format!("{MOUNT_TAG}{}", shares.len());
        let files = volume.files();
        let shared = format!("volume {} at {at}", volume.name());
        let host = utf8(&files, &shared)?;
        info.shares.push((tag.clone(), at.clone()));
        // Read-only at the device, which is the whole of what `read_only` promises.
        shares.push(
            match asked.read_only {
                true => FsDevice::new_read_only(&tag, host),
                false => FsDevice::new(&tag, host),
            }
            .map_err(|e| Error::vmm(format!("sharing {shared}"), e))?,
        );
        for mount in volume.mounts() {
            let served =
                Served::new(Arc::new(Remote::new(&mount.source)?)).map_err(|e| Error::Io {
                    context: format!("serving the mount at {} in {}", mount.path, volume.name()),
                    source: e,
                })?;
            let tag = format!("{MOUNT_TAG}{}", shares.len());
            info.shares
                .push((tag.clone(), format!("{at}/{}", mount.path)));
            shares.push(
                FsDevice::new_external(&tag, Arc::new(served))
                    .map_err(|e| Error::vmm(format!("serving {tag}"), e))?,
            );
        }
    }

    // What PID 1 does once the guest is up: run the agent from the boot root, in the image's
    // environment, and end the machine when it does. The init binary and this config are
    // injected into the root through the overlay below, and `apply` is what appends
    // `init=/init.krun` to the kernel cmdline.
    let init = krun_init::Config::builder()
        .args(&[&format!("/{AGENT_NAME}")])
        .env(&env.iter().map(String::as_str).collect::<Vec<_>>());

    // Leaked, because the root borrows it for as long as the machine runs, which is the rest
    // of the process.
    let init: &'static krun_init::Config = Box::leak(Box::new(init.build()));

    let published = published(&machine.ports)?;
    let addresses: Vec<_> = published
        .iter()
        .map(|(port, listener)| Ok((*port, listener.local_addr()?)))
        .collect::<io::Result<_>>()
        .map_err(|e| Error::io("reading where the published ports are", e))?;

    // The guest kernel, which libkrun `dlopen`s rather than links.
    let mut payload =
        Payload::load_krunfw().map_err(|e| Error::vmm("loading the guest kernel", e))?;
    // No bounce buffer. A guest whose RAM reaches past 4 GiB -- the default 4 GiB does, over
    // the MMIO hole -- sets aside 64 MiB for swiotlb and zeroes it while it boots, and no
    // device here bounces through it: none offers `VIRTIO_F_ACCESS_PLATFORM`, so virtio
    // reaches guest memory directly, and nothing is passed through. Each page the guest first
    // touches is a fault its hypervisor resolves: 13 ms of a boot under nested KVM, and under
    // WHP, nested, 2.1s of 6.4.
    payload.append_cmdline("swiotlb=noforce");
    // The kernel's crypto algorithms are not tested as each registers: on one vCPU those tests
    // are a third of a boot, run against the init for it.
    #[cfg(target_arch = "aarch64")]
    payload.append_cmdline("cryptomgr.notests");

    let mut overlay = FsOverlay::new();
    for dir in ROOT_DIRS {
        overlay
            .add_dir(dir, DIR_MODE)
            .map_err(|e| Error::vmm(format!("making /{dir} on the boot root"), e))?;
    }
    overlay
        .add_file(AGENT_NAME, AGENT, 0o755, false)
        .map_err(|e| Error::vmm("putting the agent on the boot root", e))?;
    init.apply(&mut overlay, &mut payload)
        .map_err(Error::Init)?;
    // Under nested virtualization -- a cloud instance -- each check the guest kernel makes of
    // its clock, and each PCI slot it probes, is a trip to the outer hypervisor: some 70 ms of
    // a boot there. Its clock is its hypervisor's, which libkrun has it prefer by not saying the
    // TSC is invariant, so a check of the TSC guards nothing; and a machine without a GPU has
    // no PCI device, its devices being virtio-mmio's. x86_64's own: elsewhere the kernel would
    // not know them, and would pass them to the init as its environment.
    #[cfg(target_arch = "x86_64")]
    {
        payload.append_cmdline("tsc=reliable no_timer_check");
        if !machine.spec.gpu {
            payload.append_cmdline("pci=off");
        }
    }

    let mut boot_root =
        FsDevice::new_null(ROOT_TAG).map_err(|e| Error::vmm("making the boot root", e))?;
    boot_root.set_overlay(overlay);

    // The guest's console goes to libkrun's log, and the agent's port to two pipes: one the
    // host's messages are written into for the agent to read, one it writes into for this
    // process to pass on. The I/O port goes to the host's own two. Their port ends leaked,
    // because the console borrows them for the rest of the process.
    let (to_guest, for_guest) = io::pipe().map_err(|e| Error::io("making the agent's input", e))?;
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
    let rootfs = utf8(&machine.rootfs, "the root's image")?;
    let mut lower = BlockDevice::new("vda", rootfs, DiskFormat::Vmdk)
        .map_err(|e| Error::vmm("attaching the root's image", e))?;
    lower.set_read_only(true);

    let upper_path = utf8(&machine.upper, "the upper disk")?;
    // A sandbox's disk as the embedder's disks open it ([`crate::disks`]): read and written
    // through what they make of it, if anything, rather than as the file is.
    let opened = match &machine.sandbox_disk {
        Some(disk) => crate::disks::disks()
            .open(&machine.upper, disk.source.as_deref())
            .map_err(|e| Error::io("opening the upper disk", e))?,
        None => None,
    };
    let upper_device = match &opened {
        Some(disk) => BlockDevice::with_storage("vdb", upper_path, DiskFormat::Raw, disk.storage()),
        None => BlockDevice::new("vdb", upper_path, DiskFormat::Raw),
    }
    .map_err(|e| Error::vmm("attaching the upper disk", e))?;
    // Told its machine has booted, as its agent says it is ready.
    let booting = opened.clone();
    if let Some(disk) = opened {
        std::thread::spawn(move || disk.background());
    }

    let mut devices = MmioDeviceManager::new();
    devices.add(boot_root);
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

    // The whole of what having neither is: no device -- see `net`.
    if machine.spec.network || !published.is_empty() || machine.client_ports.is_some() {
        devices.add(net::device(
            machine.spec.network,
            published,
            machine.client_ports.clone(),
        )?);
    }

    // In this order, and the order is the whole of what names them -- see the module
    // doc.
    devices.add(lower);
    devices.add(upper_device);

    for share in shares {
        devices.add(share);
    }

    let vmm = VmmBuilder::new()
        .vcpus(machine.spec.vcpus)
        .map_err(|e| Error::vmm(format!("{} vCPUs", machine.spec.vcpus), e))?
        .ram_mib(machine.spec.ram_mib)
        .map_err(|e| Error::vmm(format!("{} MiB of memory", machine.spec.ram_mib), e))?;
    // A guest that powers off ends the machine: on x86_64 through the sleep register ACPI
    // names, without which it halts and is ended only once it has been silent; aarch64 through
    // PSCI. The Windows guest kernel has no ACPI.
    #[cfg(all(target_arch = "x86_64", target_os = "linux"))]
    let vmm = vmm
        .acpi(true)
        .map_err(|e| Error::vmm("describing the machine in ACPI", e))?;
    // A machine snapshotted as its agent waits, or restored from such a snapshot: one snapshot
    // per shape beside the sandboxes' disks, which the first machine of it takes as it boots on
    // and every later one is restored from, but for a shape whose restores do not come up.
    // `VIRTX_SNAPSHOT_SAVE` and `VIRTX_SNAPSHOT_RESTORE` name one directory to take or to
    // restore from instead.
    let (mut save, mut restore) = (
        std::env::var_os("VIRTX_SNAPSHOT_SAVE").map(std::path::PathBuf::from),
        std::env::var_os("VIRTX_SNAPSHOT_RESTORE").map(std::path::PathBuf::from),
    );
    // The shape's own snapshot, where this machine takes it or is restored from it.
    let mut shared = None;
    if save.is_none()
        && restore.is_none()
        && let Some(dir) = snapshot_shape(&machine, &info)
    {
        (save, restore) = shape_snapshot(&dir);
        if save.is_some() || restore.is_some() {
            shared = Some(dir);
        }
    }
    let save_and_boot = shared.is_some() && save.is_some();
    let restored = shared.clone().filter(|_| restore.is_some());
    // Read only where a snapshot is taken, on x86_64 Linux.
    #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
    let _ = save_and_boot;
    let vmm = vmm.payload(payload).devices(devices);
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    let vmm = match &restore {
        Some(dir) => {
            if restored.is_some() {
                // Restored from now: what `sweep_snapshots` leaves for another `SNAPSHOT_KEPT`.
                let _ =
                    File::open(dir).and_then(|dir| dir.set_modified(std::time::SystemTime::now()));
                let _ = File::create(restoring(dir, std::process::id()));
            }
            vmm.restore(dir)
        }
        None => vmm,
    };
    let vmm = vmm
        .prefault_memory(prefault_memory())
        .build()
        .map_err(|e| {
            // A shape's snapshot this host cannot restore is not restored again.
            #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
            if let Some(dir) = &restored {
                snapshot_broken(dir);
            }
            Error::vmm("building the machine", e)
        })?;
    let handle = vmm
        .handle()
        .map_err(|e| Error::vmm("a handle on the machine", e))?;

    // The root for the agent to build, then the host's messages on to it, and the agent's back -- `Ready` with where the ports
    // are published, which only this side knows. A host that has closed `ctrlin`, or written
    // something unreadable into it, is gone and will hear nothing more: the guest is shut down
    // as the host would, killed if it has not ended by `STOP_TIMEOUT`. An agent that has
    // stopped, or said something unreadable, takes no more commands, and the machine ends so
    // the host sees it has.
    //
    // Once the agent is up it is pinged, and a guest it has said nothing from for
    // `guest_silence` -- halted, or too stuck to answer -- is ended, as one that powered off.
    // The check writes nothing: a guest that reads nothing can leave a write waiting.
    let mut ctrlin = ctrlin;
    let for_guest = Arc::new(Mutex::new(for_guest));
    let heard = Arc::new(Mutex::new(None::<Instant>));
    std::thread::spawn({
        let for_guest = Arc::clone(&for_guest);
        let saving = save.is_some();
        let info = info.clone();
        move || {
            if !saving {
                let _ = wire::send(&mut *for_guest.lock().unwrap(), &info);
            }
            while let Ok(Some(message)) = wire::recv::<CtrlReq>(&mut ctrlin) {
                let _ = wire::send(&mut *for_guest.lock().unwrap(), &message);
            }
            let _ = wire::send(&mut *for_guest.lock().unwrap(), &CtrlReq::Shutdown);
            std::thread::sleep(STOP_TIMEOUT);
            std::process::exit(1);
        }
    });
    std::thread::spawn({
        let heard = Arc::clone(&heard);
        let for_info = Arc::clone(&for_guest);
        let info_after = info.clone();
        let rootfs = machine.rootfs.clone();
        move || {
            // Sent only after a snapshot, on x86_64 Linux.
            #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
            let _ = (&for_info, &info_after, &rootfs, &restored);
            while let Ok(Some(message)) = wire::recv::<CtrlResp>(&mut from_guest) {
                if let CtrlResp::Waiting = message {
                    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
                    if let Some(dir) = &save {
                        let started = Instant::now();
                        if !save_and_boot {
                            match handle.snapshot(dir) {
                                Ok(()) => log::warn!("snapshot written in {:?}", started.elapsed()),
                                Err(e) => log::error!("snapshot: {e}"),
                            }
                            std::process::exit(0);
                        }
                        // Written aside and moved into place whole, so a machine restored from
                        // it never reads one half written; the first of two to finish wins.
                        let aside = dir.with_extension(format!("taking-{}", std::process::id()));
                        match handle.snapshot(&aside) {
                            Ok(()) => {
                                use std::os::unix::ffi::OsStrExt;
                                let named = std::fs::write(
                                    aside.join("rootfs"),
                                    rootfs.as_os_str().as_bytes(),
                                );
                                if named.is_err() || std::fs::rename(&aside, dir).is_err() {
                                    let _ = std::fs::remove_dir_all(&aside);
                                }
                                sweep_snapshots(dir);
                            }
                            Err(e) => {
                                log::error!("snapshot: {e}");
                                let _ = std::fs::remove_dir_all(&aside);
                            }
                        }
                        let _ = handle.resume();
                        let _ = wire::send(&mut *for_info.lock().unwrap(), &info_after);
                    }
                    let _ = &handle;
                    continue;
                }
                *heard.lock().unwrap() = Some(Instant::now());
                let _ = match message {
                    CtrlResp::Ready(_) => {
                        if let Some(disk) = &booting {
                            disk.booted();
                        }
                        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
                        if let Some(dir) = &restored {
                            restore_ready(dir);
                        }
                        send(&CtrlResp::Ready(addresses.clone()))
                    }
                    CtrlResp::Pong => Ok(()),
                    message => send(&message),
                };
            }
            std::process::exit(1);
        }
    });
    std::thread::spawn({
        let heard = Arc::clone(&heard);
        move || {
            loop {
                std::thread::sleep(PING_EVERY);
                if heard.lock().unwrap().is_some() {
                    let _ = wire::send(&mut *for_guest.lock().unwrap(), &CtrlReq::Ping);
                }
            }
        }
    });
    // A machine paused whole -- this process stopped, as the host does while it copies its
    // disk -- is not a silent guest: this thread stopped with it, and wakes well past its second,
    // which starts the silence anew.
    let silence = guest_silence();
    std::thread::spawn(move || {
        let mut checked = Instant::now();
        loop {
            std::thread::sleep(Duration::from_secs(1));
            let now = Instant::now();
            let mut heard = heard.lock().unwrap();
            if now.duration_since(checked) > PAUSED_PAST {
                if heard.is_some() {
                    *heard = Some(now);
                }
            } else if heard.is_some_and(|heard| heard.elapsed() > silence) {
                log::error!("the guest has answered nothing for {silence:?}: it is ended");
                std::process::exit(1);
            }
            checked = now;
        }
    });

    // Held for as long as the machine runs -- which is the rest of the process, whose end is
    // what lets them go.
    let _held = holds;
    vmm.run();
    Err(Error::EventLoop)
}

/// Where a snapshot of a machine of this one's shape is kept -- beside the sandboxes' disks,
/// `snapshots/<host>/<shape>`. The host is this binary, whose libkrun and agent write it, with the
/// CPU and kernel whose KVM it names the features of; the shape is everything a restored guest
/// has to find as it was: its vCPUs, memory, devices, root image and upper disk's size. None off
/// Linux x86_64 and for a GPU's machine.
fn snapshot_shape(machine: &MachineConfig, info: &wire::MachineInfo) -> Option<std::path::PathBuf> {
    use std::hash::{Hash, Hasher};
    if !cfg!(all(target_os = "linux", target_arch = "x86_64")) || machine.spec.gpu {
        return None;
    }
    let home = machine.upper.parent()?.parent()?;
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    machine.spec.vcpus.hash(&mut hash);
    machine.spec.ram_mib.hash(&mut hash);
    (machine.spec.network || !machine.ports.is_empty()).hash(&mut hash);
    info.shares.hash(&mut hash);
    machine.rootfs.hash(&mut hash);
    std::fs::metadata(&machine.upper)
        .ok()?
        .len()
        .hash(&mut hash);
    let shape = hash.finish();
    let exe = std::fs::metadata(std::env::current_exe().ok()?).ok()?;
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    (exe.len(), exe.modified().ok()).hash(&mut hash);
    std::fs::read("/proc/sys/kernel/osrelease")
        .ok()?
        .hash(&mut hash);
    // The first CPU's, which every other's matches, read alone: the whole file is slow to make
    // on a host of many.
    use std::io::BufRead;
    io::BufReader::new(File::open("/proc/cpuinfo").ok()?)
        .lines()
        .map_while(|line| line.ok())
        .take_while(|line| !line.is_empty())
        .filter(|line| {
            [
                "vendor_id",
                "cpu family",
                "model",
                "stepping",
                "microcode",
                "flags",
            ]
            .iter()
            .any(|key| line.split(':').next().is_some_and(|k| k.trim() == *key))
        })
        .for_each(|line| line.hash(&mut hash));
    let dir = home
        .join("snapshots")
        .join(format!("{:016x}", hash.finish()));
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join(format!("{shape:016x}")))
}

/// What a machine does with its shape's snapshot at `dir`, as `(save, restore)`: takes it as it
/// boots on, is restored from it, or neither once its restores have not come up.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn shape_snapshot(dir: &Path) -> (Option<std::path::PathBuf>, Option<std::path::PathBuf>) {
    if dir.join(SNAPSHOT_BROKEN).exists() {
        (None, None)
    } else if !dir.join("state").exists() {
        (Some(dir.to_owned()), None)
    } else if restores_lost(dir) {
        snapshot_broken(dir);
        (None, None)
    } else {
        (None, Some(dir.to_owned()))
    }
}

/// Off x86_64 Linux there is no snapshot, nor a shape that names one.
#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
fn shape_snapshot(_: &Path) -> (Option<std::path::PathBuf>, Option<std::path::PathBuf>) {
    (None, None)
}

/// Beside a snapshot whose restores do not come up, which a machine of its shape then boots
/// without and takes none of.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const SNAPSHOT_BROKEN: &str = "broken";

/// How many restores of one snapshot may end before their agent is ready, with none ready since,
/// before it is taken for broken: one cut short as its sandbox closed is not.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const RESTORES_LOST: usize = 3;

/// Named in a snapshot by the process restoring it, until its agent is ready.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn restoring(dir: &Path, pid: u32) -> std::path::PathBuf {
    dir.join(format!("restoring-{pid}"))
}

/// The restores of `dir` whose process has ended before their agent was ready.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn lost_restores(dir: &Path) -> Vec<std::path::PathBuf> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .and_then(|name| name.strip_prefix("restoring-"))
                .and_then(|pid| pid.parse::<libc::pid_t>().ok())
                // SAFETY: signal 0 is only asked whether the process is there.
                .is_some_and(|pid| unsafe { libc::kill(pid, 0) } != 0)
        })
        .map(|entry| entry.path())
        .collect()
}

/// Whether `dir`'s restores keep ending before their agent is ready.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn restores_lost(dir: &Path) -> bool {
    lost_restores(dir).len() >= RESTORES_LOST
}

/// A restore of `dir` has come up: it and those lost before it are forgotten.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn restore_ready(dir: &Path) {
    let _ = std::fs::remove_file(restoring(dir, std::process::id()));
    for lost in lost_restores(dir) {
        let _ = std::fs::remove_file(lost);
    }
}

/// Marks `dir` broken and removes all of it but its root image's name, which `sweep_snapshots`
/// reads.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn snapshot_broken(dir: &Path) {
    log::error!("snapshot {}: its restores do not come up", dir.display());
    let _ = File::create(dir.join(SNAPSHOT_BROKEN));
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let name = entry.file_name();
        if name != "rootfs" && name != SNAPSHOT_BROKEN {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// How long a snapshot no machine has been restored from is kept once none can be: the
/// restores a server of another binary sharing the home may still be making from it.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const SNAPSHOT_KEPT: Duration = Duration::from_secs(60 * 60);

/// Removes, beside `taken`, the snapshots no machine here can be restored from -- another
/// binary's or host's, one whose root image is gone, one left half written -- once none has been
/// taken or restored from for `SNAPSHOT_KEPT`.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn sweep_snapshots(taken: &Path) {
    use std::os::unix::ffi::OsStrExt;
    let Some(ours) = taken.parent() else { return };
    let Some(snapshots) = ours.parent() else {
        return;
    };
    let entries = |dir: &Path| {
        std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .collect::<Vec<_>>()
    };
    for binary in entries(snapshots) {
        for snapshot in entries(&binary) {
            let idle = std::fs::metadata(&snapshot)
                .and_then(|meta| meta.modified())
                .ok()
                .and_then(|at| at.elapsed().ok())
                .is_some_and(|idle| idle > SNAPSHOT_KEPT);
            let unusable = binary != ours
                || snapshot.extension().is_some()
                || std::fs::read(snapshot.join("rootfs"))
                    .is_ok_and(|rootfs| !Path::new(std::ffi::OsStr::from_bytes(&rootfs)).exists());
            if idle && unusable {
                let _ = std::fs::remove_dir_all(&snapshot);
            }
        }
        if binary != ours {
            let _ = std::fs::remove_dir(&binary);
        }
    }
}

/// 64 bytes of the host's randomness, or none where it has none to read.
fn host_seed() -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::io::Read;
        let mut seed = vec![0; 64];
        if File::open("/dev/urandom")
            .and_then(|mut random| random.read_exact(&mut seed))
            .is_ok()
        {
            return seed;
        }
    }
    Vec::new()
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
        log::set_max_level(log::LevelFilter::Warn);
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
        let message = record.args().to_string();
        // A terminal ends its lines with `\r\n`, and libkrun takes off only the `\n`.
        let message = match record.target() {
            CONSOLE_TARGET => message.trim_end_matches('\r').to_string(),
            _ => message,
        };
        let _ = send(&CtrlResp::Log {
            level: record.level().to_string(),
            target: record.target().to_string(),
            message,
        });
    }

    fn flush(&self) {}
}
/// A listener on this host's loopback for each of `ports`, which connections to that guest port
/// are relayed from. Refused for `0`, which is no port, and for one given twice.
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
            context: format!("publishing guest port {port}"),
            source: e,
        })?;
        published.push((port, listener));
    }
    Ok(published)
}

/// The path of `what`, such as `/abin`, as libkrun takes it, which is a `&str`. Refused rather
/// than lossily converted: a name with a replacement character in it opens some other file or
/// none.
fn utf8<'a>(path: &'a Path, what: &str) -> Result<&'a str> {
    path.to_str().ok_or_else(|| Error::Io {
        context: format!("the path of {what} is not valid UTF-8, which libkrun cannot name"),
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
    executable(&dir, false);
    Some(dir)
}

/// Mark each file in `dir` executable for the guest, and with `deep` each file under it: `/abin`'s
/// programs, and what they install beside themselves, which they run by path too, such as
/// `browser`'s Chromium. `alib/`'s directories hold libraries, which the guest's loader maps
/// whatever their mode, and many of them, so only its top level is marked.
///
/// libkrun's virtiofs on Windows answers `0644` for a regular file unless an alternate data
/// stream beside it, in libkrun's own `uid:gid:0mode` form, says otherwise -- and NTFS keeps no
/// mode for an unpacked release to have brought. Every boot, so a directory replaced since is
/// marked again. A file it fails on is said and skipped, as a host with no `alib/` is a machine
/// without it.
#[cfg(windows)]
fn executable(dir: &Path, deep: bool) {
    const OVERRIDE_STAT: &str = ":user.containers.override_stat";
    const ROOT_EXECUTABLE: &str = "0:0:0100755";

    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for path in entries.flatten().map(|entry| entry.path()) {
        if deep && path.is_dir() {
            executable(&path, true);
        }
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

/// The directory the image's process starts in -- a machine's, and a build step's: its config's
/// `WorkingDir`, or the root where the config names none -- unset, or set empty, as images built
/// by older tools leave it.
pub(crate) fn workdir(config: &Config) -> String {
    config
        .working_dir()
        .as_deref()
        .filter(|dir| !dir.is_empty())
        .unwrap_or("/")
        .to_owned()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_working_dir_the_image_left_empty_or_unset_is_the_root() {
        let mut set = Config::default();
        set.set_working_dir(Some("/app".to_string()));
        assert_eq!(workdir(&set), "/app");
        let mut empty = Config::default();
        empty.set_working_dir(Some(String::new()));
        assert_eq!(workdir(&empty), "/");
        assert_eq!(workdir(&Config::default()), "/");
    }

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
