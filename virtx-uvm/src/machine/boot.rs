//! The machine from inside the process that becomes it: what [`enter`](super::enter) builds.
//!
//! # The devices, and what each is for
//!
//! ```text
//! virtio-fs  /dev/root      an empty root, holding the init and the directories it mounts
//!                           over -- replaced by the overlay before anything else runs
//! virtio-blk /dev/vda       the root filesystem's disk, read-only -- the overlay's lower
//! virtio-blk /dev/vdb       the snapshot -- the overlay's upper layer
//! virtio-con hvc0           the guest's console, into libkrun's log, which goes to ctlout
//!            krun-stdin     the command's stdin, stdout and stderr: this process's own
//!            krun-stdout
//!            krun-stderr
//! virtio-gpu                Vulkan, on a machine that asked for it and a build that has it
//! virtio-vsock              the guest's network, on a machine with one or with published
//!                           ports -- see [`Network`]
//! virtio-fs  mnt0, mnt1     one per share of a volume, mounted by tag after the pivot
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
//! # The host
//!
//! Apart from the command's stdio, everything this process tells the host goes on `ctlout`, a
//! frame at a time ([`send`]): where the ports are published, libkrun's log -- the guest's
//! console among it -- and why the machine could not be built. `ctlin` is watched for the host
//! telling it to stop, or going away ([`hold`]).

use std::convert::Infallible;
use std::fs::File;
use std::io::{self, Stderr, Stdin, Stdout};
use std::net::{Ipv4Addr, TcpListener};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

use krun::{
    BalloonDevice, BlockDevice, ConsoleDevice, DiskFormat, FsDevice, FsOverlay, MmioDeviceManager,
    Payload, RngDevice, VmmBuilder,
};
use oci_spec::image::Config;

#[cfg(feature = "gpu")]
use super::gpu;
use super::wire::{self, HostMessage, MachineMessage};
use super::{Error, Machine, Network, Result, net};
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

/// The log target libkrun gives what the guest's console prints.
const CONSOLE_TARGET: &str = "init_or_kernel";

/// `ctlout`, once this process [holds](hold) it.
static CTLOUT: OnceLock<Mutex<File>> = OnceLock::new();

/// Build `machine`, running `cmd`, and become it: load the guest kernel,
/// lay the init over an empty root, attach the devices, and run.
///
/// **This does not return.** The VMM exits this process with the guest's own status, so what
/// comes back is a machine that could not be built, or a failure of the event loop.
///
/// On macOS the calling binary needs the `com.apple.security.hypervisor` entitlement.
pub(super) fn boot(machine: Machine, cmd: &[String]) -> Result<Infallible> {
    // Before anything is built, so a machine over a snapshot another one holds is refused
    // with nothing to undo.
    let lock = machine.snapshot.as_ref().map(Snapshot::lock).transpose()?;

    // What PID 1 does once the guest is up: run `cmd`, and end the machine with its exit
    // code. The init binary and this config are injected into the root through the overlay
    // below, and `apply` is what appends `init=/init.krun` to the kernel cmdline. The two
    // devices go in as environment because that is how libkrun's init is told where its root
    // is.
    let env = env(&machine.config);
    let mut init = krun_init::Config::builder()
        .args(&cmd.iter().map(String::as_str).collect::<Vec<_>>())
        .env(&env.iter().map(String::as_str).collect::<Vec<_>>())
        .workdir(machine.config.working_dir().as_deref().unwrap_or("/"))
        .env_var(&format!("KRUN_BLOCK_LOWER_DEVICE={LOWER_DEVICE}"))
        .env_var(&format!("KRUN_BLOCK_LOWER_FSTYPE={LOWER_FSTYPE}"))
        .env_var(&format!("KRUN_BLOCK_UPPER_DEVICE={UPPER_DEVICE}"))
        .env_var(&format!("KRUN_BLOCK_UPPER_FSTYPE={UPPER_FSTYPE}"));
    if machine.network != Network::None {
        init = init.env_var(&format!("{}={}", net::NAMESERVER_ENV, net::NAMESERVER));
    }

    // The virtio-fs devices the guest is shown, which the init mounts by their tags in this
    // order: each volume's files and the stores under them, with each volume held. Refused
    // where one could not be shown where it was asked.
    let mut shares = Vec::new();
    let mut holds = Vec::with_capacity(machine.volumes.len());
    for (i, asked) in machine.volumes.iter().enumerate() {
        let refused = |why| {
            Err(Error::MountPoint {
                path: asked.path.clone(),
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

        let volume = crate::Volume::open(&asked.path)?;
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

    // Leaked, because the root borrows it for as long as the machine runs, which is the rest
    // of the process.
    let init: &'static krun_init::Config = Box::leak(Box::new(init.build()));

    let published = published(&machine.ports)?;
    let addresses = published
        .iter()
        .map(|(port, listener)| Ok((*port, listener.local_addr()?)))
        .collect::<io::Result<_>>()
        .map_err(|e| Error::io("reading where the published ports are", e))?;
    send(&MachineMessage::Published(addresses))
        .map_err(|e| Error::io("telling the host where the published ports are", e))?;

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

    let mut root =
        FsDevice::new_null(ROOT_TAG).map_err(|e| Error::vmm("making the boot root", e))?;
    root.set_overlay(overlay);

    // The command's stdin, stdout and stderr are this process's, each a port of its own that
    // the init hands the command, and the console goes to libkrun's log -- as libkrun does for
    // streams that are not a terminal. Leaked, because the console borrows them for the rest
    // of the process.
    let stdin: &'static Stdin = Box::leak(Box::new(io::stdin()));
    let stdout: &'static Stdout = Box::leak(Box::new(io::stdout()));
    let stderr: &'static Stderr = Box::leak(Box::new(io::stderr()));
    #[cfg(unix)]
    let [stdin, stdout, stderr] = {
        use std::os::fd::AsFd;
        [stdin.as_fd(), stdout.as_fd(), stderr.as_fd()]
    };
    #[cfg(windows)]
    let [stdin, stdout, stderr] = {
        use std::os::windows::io::AsHandle;
        [stdin.as_handle(), stdout.as_handle(), stderr.as_handle()]
    };
    let mut console = ConsoleDevice::builder();
    console
        .add_default_console(Some(stdin), Some(stdout), Some(stderr))
        .map_err(|e| Error::vmm("wiring the command's stdio", e))?;

    // Read-only at the device and not merely in the guest's mount flags: what is behind
    // it is the layer files, shared with every other root filesystem over them.
    let rootfs = utf8(&machine.rootfs)?;
    let mut lower = BlockDevice::new("vda", rootfs, DiskFormat::Vmdk)
        .map_err(|e| Error::vmm(format!("attaching {rootfs}"), e))?;
    lower.set_read_only(true);

    let upper_path = utf8(&upper)?;
    let upper_device = BlockDevice::new("vdb", upper_path, DiskFormat::Raw)
        .map_err(|e| Error::vmm(format!("attaching {upper_path}"), e))?;

    let mut devices = MmioDeviceManager::new();
    devices.add(root);
    devices.add(
        console
            .build()
            .map_err(|e| Error::vmm("building the guest console", e))?,
    );
    devices.add(RngDevice::new().map_err(|e| Error::vmm("attaching an entropy source", e))?);
    devices.add(BalloonDevice::new().map_err(|e| Error::vmm("attaching a balloon", e))?);

    #[cfg(feature = "gpu")]
    if machine.gpu {
        devices.add(gpu::device(machine.gpu_memory_mib)?);
    }

    // The whole of what having neither is: no device -- see `net`.
    if machine.network != Network::None || !published.is_empty() {
        devices.add(net::attach(machine.network, published)?);
    }

    // In this order, and the order is the whole of what names them -- see the module
    // doc.
    devices.add(lower);
    devices.add(upper_device);

    for share in shares {
        devices.add(share);
    }

    let vmm = VmmBuilder::new()
        .vcpus(machine.vcpus)
        .map_err(|e| Error::vmm(format!("{} vCPUs", machine.vcpus), e))?
        .ram_mib(machine.ram_mib)
        .map_err(|e| Error::vmm(format!("{} MiB of memory", machine.ram_mib), e))?
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

    // Held for as long as the machine runs -- which is the rest of the process, whose end is
    // what lets them go.
    let _held = (lock, holds);
    vmm.run();
    Err(Error::EventLoop)
}

/// Take the ends the host left this process: from here on everything it says goes to
/// `ctlout`, libkrun's log among it, and the process ends when `ctlin` says so or closes.
///
/// # Safety
///
/// `ctlin` and `ctlout` are ends the host left open for this process, which nothing else in it
/// holds.
pub(super) unsafe fn hold(ctlin: u64, ctlout: u64) {
    // SAFETY: the caller's.
    let (mut ctlin, ctlout) = unsafe { (from_raw(ctlin), from_raw(ctlout)) };
    let _ = CTLOUT.set(Mutex::new(ctlout));
    if log::set_logger(&Logger).is_ok() {
        log::set_max_level(log::LevelFilter::Warn);
    }
    std::thread::spawn(move || match wire::recv(&mut ctlin) {
        // Closed, or unreadable, is a host that is gone and will hear nothing more.
        Ok(Some(HostMessage::Stop)) | Ok(None) | Err(_) => std::process::exit(1),
    });
}

/// Tell the host `message`.
pub(super) fn send(message: &MachineMessage) -> io::Result<()> {
    let ctlout = CTLOUT
        .get()
        .ok_or_else(|| io::Error::other("ctlout is not held"))?;
    wire::send(&mut *ctlout.lock().unwrap(), message)
}

#[cfg(unix)]
unsafe fn from_raw(end: u64) -> File {
    // SAFETY: the caller's.
    unsafe { std::os::fd::FromRawFd::from_raw_fd(end as i32) }
}

#[cfg(windows)]
unsafe fn from_raw(end: u64) -> File {
    // SAFETY: the caller's.
    unsafe { std::os::windows::io::FromRawHandle::from_raw_handle(end as usize as _) }
}

/// libkrun's log, each record a message on `ctlout` as it is made: in the thread that makes
/// it, so what the console's last lines are is out before libkrun ends the process.
struct Logger;

impl log::Log for Logger {
    fn enabled(&self, _: &log::Metadata) -> bool {
        true
    }

    fn log(&self, record: &log::Record) {
        let message = match record.target() {
            // A terminal ends its lines with `\r\n`, and libkrun takes off only the `\n`.
            CONSOLE_TARGET => {
                MachineMessage::Console(record.args().to_string().trim_end_matches('\r').into())
            }
            target => MachineMessage::Log {
                level: record.level().to_string(),
                message: format!("{target}: {}", record.args()),
            },
        };
        let _ = send(&message);
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

/// The environment the guest runs in: the image's, with `PATH` and `HOME` added when it
/// sets neither, with the values docker uses.
fn env(config: &Config) -> Vec<String> {
    let mut env = config.env().clone().unwrap_or_default();
    let set = |env: &[String], name: &str| env.iter().any(|var| var.starts_with(name));
    if !set(&env, "PATH=") {
        env.push(DEFAULT_PATH.to_string());
    }
    if !set(&env, "HOME=") {
        env.push("HOME=/root".to_string());
    }
    env
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_the_image_did_not_set_is_added_and_one_it_did_is_left_alone() {
        let mut stated = Config::default();
        stated.set_env(Some(vec!["PATH=/opt/bin".to_string()]));
        let stated = env(&stated);
        assert!(stated.contains(&"PATH=/opt/bin".to_string()), "{stated:?}");
        assert!(!stated.iter().any(|var| var == DEFAULT_PATH), "{stated:?}");

        let silent = env(&Config::default());
        assert!(silent.iter().any(|var| var == DEFAULT_PATH), "{silent:?}");
        assert!(silent.contains(&"HOME=/root".to_string()), "{silent:?}");
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
