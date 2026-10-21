//! A machine: a root filesystem booted with a writable layer over it, running what it is
//! told to.
//!
//! # What it takes
//!
//! Two disks, and neither is made here:
//!
//! ```text
//!   a Rootfs   ──► <chain>.vmdk    the image, read-only, shared by every session over it
//!   an Upper   ──► session.ext4    this session's, written to
//!                        │
//!                        ▼
//!                     a machine, which does not come back
//! ```
//!
//! The root filesystem is whoever made it's -- a cache keeps one per chain of layers -- and
//! the upper layer is made once per session by whoever wants to keep it; a session given
//! none makes itself a throwaway. The stacking of the two is neither end's: it happens in
//! the guest, where overlayfs is.
//!
//! # The devices, and what each is for
//!
//! ```text
//! virtio-fs  /dev/root   an empty root, holding the init and the directories it mounts
//!                        over -- replaced by the overlay before anything else runs
//! virtio-blk /dev/vda    the root filesystem's disk, read-only -- the overlay's lower
//! virtio-blk /dev/vdb    this session's ext4 -- the overlay's upper layer
//! virtio-con hvc0        the guest's console, on this process's own stdin and stdout
//! virtio-vsock           the guest's network, on a session that asked for one -- see below
//! virtio-gpu             Vulkan, on a session that asked for it and a build that has it
//! virtio-fs  mnt0, mnt1  one per [`Mount`], mounted by tag on the root after the pivot
//! ```
//!
//! The mounts go on last, after both disks, because what names a disk is the order the
//! disks went on and a share takes no name from it. What tells the guest to mount them is
//! the init's config rather than a device: the host says `mnt0` goes at `/data`, and the
//! init mounts it there once the overlay is the root -- see
//! `3rdparty/_patches/libkrun-85bed71-session-mounts.patch`.
//!
//! **Attach order is what fixes the two disk names**, and the guest is told which is which
//! by name: the lower goes on first and is `/dev/vda`, the upper layer second and
//! `/dev/vdb`. The pair of constants below and the order [`Machine::enter`] adds them in
//! are one statement made in two places, which is why they are next to each other.
//!
//! # The network, and where it actually is
//!
//! In this process's own sockets. The guest's kernel hands every socket it cannot answer on
//! its own stack to the vsock device, and libkrun opens the host socket that answers it --
//! so a session's network is not a thing the host has and the guest is let onto, it is
//! this process reaching out on the guest's behalf, and its `net` module is what decides where.
//!
//! A session that asked for no network gets no device, and there is nothing in the guest
//! that can put one back. See [`Machine::network`].
//!
//! # The console is the one thing that differs per host
//!
//! libkrun's public API is `std::os::fd` everywhere but Windows, where a console stream is
//! a `BorrowedHandle` rather than a `BorrowedFd`. That one call is `cfg`'d and nothing else
//! here is: a session is the same machine on every host this builds for.
//!
//! # The root that is thrown away
//!
//! The virtio-fs root is [`FsDevice::new_null`] -- a filesystem with nothing in it but what
//! the overlay puts there: the init binary, its config, and the empty directories the init
//! mounts the two disks over. Nothing on the host backs it. It exists for the moment
//! between the kernel handing over and the init pivoting onto the overlay, and after the
//! pivot it is not reachable by any path.

use std::convert::Infallible;
use std::io::{stderr, stdout};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

// The one thing here that is not the same on every host -- see the module doc.
#[cfg(unix)]
use std::io::stdin;

use krun::{
    BalloonDevice, BlockDevice, ConsoleDevice, DiskFormat, FsDevice, FsOverlay, MmioDeviceManager,
    Payload, RngDevice, VmmBuilder,
};
use oci_spec::image::Config;

use crate::filesystem::{self, Ext4, Tree};
use crate::rootfs::Rootfs;

mod channel;
pub mod doors;
#[cfg(feature = "gpu")]
mod gpu;
mod net;

pub use channel::{Channel, Incoming, Listener, Outgoing};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A mount the guest could not be told where to put. A mount point is a path the guest
    /// resolves on its own root, and one that is not absolute would land wherever the init
    /// happened to be standing.
    #[error("{at} is not absolute, which a mount point in the guest has to be")]
    MountPoint { at: String },

    /// An upper layer that could not be formatted.
    #[error(transparent)]
    Filesystem(#[from] filesystem::Error),

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

    /// The machine started and its event loop then broke -- the one failure that can
    /// arrive after a guest is running, and the only way [`Machine::enter`] comes back at
    /// all.
    ///
    /// No source, because there is none to carry: libkrun logs what went wrong and returns
    /// nothing. `KRUN_LOG=debug` is where the answer is.
    #[error("the machine's event loop stopped; libkrun logged why (try KRUN_LOG=debug)")]
    EventLoop,

    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },
}

impl Error {
    fn io(context: impl Into<String>, source: std::io::Error) -> Error {
        Error::Io {
            context: context.into(),
            source,
        }
    }

    fn vmm(context: impl Into<String>, source: krun::VmmError) -> Error {
        Error::Vmm {
            context: context.into(),
            source,
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// The tag the guest kernel mounts as `/`, which libkrun's default kernel cmdline names
/// with `rootfstype=virtiofs`.
const ROOT_TAG: &str = "/dev/root";

/// Where the two disks land in the guest, in the order [`Machine::enter`] attaches them.
const LOWER_DEVICE: &str = "/dev/vda";
const UPPER_DEVICE: &str = "/dev/vdb";

/// What each is, which the guest is told rather than left to probe: the lower is an EROFS
/// this host wrote and the upper layer an ext4 this host formatted, so there is nothing
/// here that is not already known.
const LOWER_FSTYPE: &str = "erofs";
const UPPER_FSTYPE: &str = "ext4";

/// The directories the init needs on the root before it can mount anything, since a
/// filesystem with nothing in it has nowhere to mount over.
const ROOT_DIRS: [&str; 6] = ["dev", "proc", "sys", "newroot", "lower", "upper"];

/// What each [`Mount`] is attached under, with its place in the list after it. A tag is
/// only a name the two ends agree on, and one nothing else uses is all this needs.
const MOUNT_TAG: &str = "mnt";

/// What the guest mounts a [`Mount`] as, which is the only thing a virtio-fs share can be
/// mounted as.
const MOUNT_FSTYPE: &str = "virtiofs";

/// `S_IFDIR | 0755`, as [`FsOverlay::add_dir`] wants a mode.
const DIR_MODE: u32 = 0o040_755;

/// What a `PATH`-less image gets, which is what docker gives one.
const DEFAULT_PATH: &str = "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// What a session is, beyond the root filesystem it boots: where its writes go, how big a
/// machine runs them, and what it runs.
///
/// The root filesystem and what the image says about running it are all a session cannot
/// do without, and are what [`new`](Machine::new) takes. Anything else has a default and is
/// changed by naming it, and [`enter`](Machine::enter) is what turns the whole of it into a
/// machine:
///
/// ```no_run
/// # fn demo(rootfs: virtx_uvm::rootfs::Rootfs, image: virtx_uvm::image::Image)
/// # -> Result<(), Box<dyn std::error::Error>> {
/// use virtx_uvm::filesystem::Tree;
/// use virtx_uvm::machine::{Machine, Upper};
///
/// Machine::new(rootfs, image.config)
///     .upper(Upper::create("/tmp/demo.ext4", Upper::SIZE, Tree::new())?)
///     .ram_mib(4096)
///     .command(["/bin/sh", "-c", "echo hi"])
///     .enter()?;
/// # Ok(())
/// # }
/// ```
///
/// Nothing here touches the host until [`enter`](Machine::enter) is called -- the upper
/// layer, the one part of a session that is a file on the host, comes in already made.
#[derive(Clone, Debug)]
pub struct Machine {
    rootfs: Rootfs,
    config: Option<Config>,
    upper: Option<Upper>,
    mounts: Vec<Mount>,
    vcpus: u8,
    ram_mib: u32,
    command: Vec<String>,
    /// The whole argv, for a caller who is not running the image's program at all -- see
    /// [`exec`](Machine::exec). When set it is what runs, and `command` is not consulted.
    exec: Option<Vec<String>>,
    env: Vec<String>,
    network: bool,
    /// The listeners whose connections are carried into the guest, each to one console port
    /// -- see [`publish`](Machine::publish). Shared rather than owned so that a `Machine`
    /// stays a value that clones; each is duplicated when the device is made.
    published: Vec<(u16, std::sync::Arc<std::net::TcpListener>)>,
    gpu: bool,
    gpu_memory_mib: Option<u32>,
    /// A named virtio-console port, and the descriptors behind it -- see
    /// [`port`](Machine::port). Held as numbers rather than a borrow so that a [`Machine`]
    /// stays a plain value; keeping the channel open is the caller's.
    port: Option<(String, channel::Raw)>,
}

impl Machine {
    /// The guest's RAM when [`ram_mib`](Machine::ram_mib) is not said.
    pub const DEFAULT_RAM_MIB: u32 = 2048;

    /// A default machine booting `rootfs`, run the way `config` -- the image's -- says, over
    /// an upper layer nobody keeps.
    pub fn new(rootfs: Rootfs, config: Option<Config>) -> Machine {
        Machine {
            rootfs,
            config,
            upper: None,
            mounts: Vec::new(),
            // Two, because a command and whatever is watching it are two things wanting the
            // processor at once, and one vCPU turns that into a queue.
            vcpus: 2,
            // Enough for a package install, which is the first thing anyone does.
            ram_mib: Self::DEFAULT_RAM_MIB,
            command: Vec::new(),
            exec: None,
            env: Vec::new(),
            // What a session that said nothing about it gets -- see `Machine::network`.
            network: false,
            published: Vec::new(),
            gpu: false,
            gpu_memory_mib: None,
            port: None,
        }
    }

    /// The ext4 image this session writes to, which [`Upper::open`] found or
    /// [`Upper::create`] made. Keeping
    /// the file is what carries a session on; deleting it is what throws one away.
    ///
    /// **Left unsaid, the session is a throwaway**: [`enter`](Machine::enter) makes an
    /// [`Upper::new`] and unlinks it the moment the machine has it open, so the writes
    /// go somewhere real and nothing outlives the process.
    pub fn upper(mut self, upper: Upper) -> Machine {
        self.upper = Some(upper);
        self
    }

    /// A host directory the guest can see, at the path the [`Mount`] names. Appends, so
    /// several calls are several mounts.
    ///
    /// The share is attached to the machine and the guest is told to mount it once its
    /// root is the overlay -- so a mount lands on what the workload sees, and the directory
    /// it goes on is made on the upper layer if the image shipped none.
    pub fn mount(mut self, mount: Mount) -> Machine {
        self.mounts.push(mount);
        self
    }

    /// Several [`Mount`]s at once, in the order given. Appends, the way
    /// [`mount`](Machine::mount) does.
    pub fn mounts(mut self, mounts: impl IntoIterator<Item = Mount>) -> Machine {
        self.mounts.extend(mounts);
        self
    }

    pub fn vcpus(mut self, vcpus: u8) -> Machine {
        self.vcpus = vcpus;
        self
    }

    pub fn ram_mib(mut self, ram_mib: u32) -> Machine {
        self.ram_mib = ram_mib;
        self
    }

    /// Whether the guest reaches a network, which by default it does not.
    ///
    /// **Off is an isolation nothing in the guest can undo**, because what it leaves out is
    /// a device. There is no interface to bring up and no socket family to reach the host
    /// through -- a socket the guest opens to anywhere but itself has nowhere to go. It is
    /// the default because a session that did not say it wanted to reach the network should
    /// not.
    ///
    /// On is what this process reaches, this host's loopback included -- see the `net` module
    /// for the whole of the rule, and why a name resolves in a guest whose image was never
    /// prepared for any of this.
    pub fn network(mut self, network: bool) -> Machine {
        self.network = network;
        self
    }

    /// Every connection `listener` accepts becomes one to `console` in the guest.
    ///
    /// **Bound by the caller**, because where it listens is the caller's to know and to
    /// tell whoever will connect -- a port it chose, or one the system gave it for `:0`. A
    /// connection reaches whatever in the guest listens on `console`, on whichever address,
    /// and is closed while nothing does -- which is what a machine on a network answers for
    /// a port nothing listens on.
    ///
    /// Appends. Ignored without a [`network`](Machine::network), which is the device a
    /// connection arrives through.
    pub fn publish(mut self, console: u16, listener: std::net::TcpListener) -> Machine {
        self.published
            .push((console, std::sync::Arc::new(listener)));
        self
    }

    /// Give the guest a virtio-gpu device carrying Vulkan, by way of
    /// [venus](https://docs.mesa3d.org/drivers/venus.html).
    ///
    /// **Ignored unless the crate was built with the `gpu` feature**, which is what pulls in
    /// virglrenderer and libkrun's GPU support. Without it this is a flag that changes
    /// nothing, so a caller need not `cfg` around it -- a build that cannot serve Vulkan
    /// runs the same session without it rather than refusing to start.
    ///
    /// The image has to carry the guest half either way. On Alpine that is `apk add
    /// mesa-vulkan-virtio vulkan-loader`; an image without it boots and finds no device.
    pub fn gpu(mut self, gpu: bool) -> Machine {
        self.gpu = gpu;
        self
    }

    /// How much memory the guest's GPU may hold, in mebibytes: the host-visible window its
    /// Vulkan memory is mapped through, and the heap size the device reports to the guest.
    /// `None` is the 8 GiB libkrun gives.
    ///
    /// Beside [`ram_mib`](Machine::ram_mib) and not out of it: on a GPU that shares the
    /// host's memory, all of it can be host memory in use at once. Ignored without
    /// [`gpu`](Machine::gpu).
    pub fn gpu_memory_mib(mut self, gpu_memory_mib: u32) -> Machine {
        self.gpu_memory_mib = Some(gpu_memory_mib);
        self
    }

    /// A named virtio-console port beside the guest's console, carrying whatever `channel` is
    /// the host end of.
    ///
    /// The console itself is this process's terminal and is the kernel's: it is where a
    /// panic and every `printk` land, so a protocol on it is one a single kernel warning
    /// would corrupt. A port is a second channel on the same device, found in the guest by
    /// the name given here -- `/dev/virtio-ports/<name>` where the guest has udev rules, and
    /// the `name` attribute under `/sys/class/virtio-ports` where it does not.
    ///
    /// The channel is borrowed and not taken: it has to stay open for as long as the machine
    /// runs, which is the rest of the process -- see [`enter`](Machine::enter). Replaces what
    /// an earlier call said.
    ///
    /// **A port is written to a queue nothing is reading until the guest opens it**, and the
    /// driver discards what arrives before then rather than queueing it. So a caller on the
    /// other end cannot speak first: something in the guest has to say it is there.
    pub fn port(mut self, name: impl Into<String>, channel: &Channel) -> Machine {
        self.port = Some((name.into(), channel.raw()));
        self
    }

    /// What to run, in place of what the image states as its `Cmd`.
    ///
    /// **The image's `Entrypoint` still goes in front of it**, which is what docker does
    /// with the same words. Left unsaid is the image's own `Cmd`, and an image that states
    /// neither runs `/bin/sh`.
    ///
    /// Replaces what an earlier call said, since a command is one argv.
    pub fn command<S: Into<String>>(mut self, command: impl IntoIterator<Item = S>) -> Machine {
        self.command = command.into_iter().map(Into::into).collect();
        self
    }

    /// Run exactly this, in place of the image's `Entrypoint` **and** its `Cmd`.
    ///
    /// For a caller running something that is not the image's program at all -- an agent
    /// put into the guest over a share, which an image stating `ENTRYPOINT ["python"]`
    /// would otherwise run as python's argument. The image's environment and working
    /// directory still apply.
    ///
    /// Replaces what an earlier call said, and takes precedence over
    /// [`command`](Machine::command) whichever order the two were called in.
    pub fn exec<S: Into<String>>(mut self, argv: impl IntoIterator<Item = S>) -> Machine {
        self.exec = Some(argv.into_iter().map(Into::into).collect());
        self
    }

    /// Environment on top of the image's, each `KEY=value`. A name the image already set is
    /// set twice and the later one wins, so this overrides rather than merges. Appends.
    pub fn env<S: Into<String>>(mut self, env: impl IntoIterator<Item = S>) -> Machine {
        self.env.extend(env.into_iter().map(Into::into));
        self
    }

    /// One `KEY=value`, for the caller with one to add.
    pub fn env_var(mut self, var: impl Into<String>) -> Machine {
        self.env.push(var.into());
        self
    }

    /// Boot the root filesystem with this session's upper layer over it, run what it says,
    /// and become the machine that does.
    ///
    /// **This does not return.** The way a session ends is *this process* ending: the VMM
    /// exits with the guest's own status, so a session asked for `exit 42` leaves the shell
    /// a 42 and everything a caller writes after the call is unreachable. What comes back
    /// is only ever a failure to get that far, which is what the [`Infallible`] says.
    ///
    /// That the status arrives at all takes a patch: the guest reports it as an ioctl on
    /// the virtiofs mount, and a session has pivoted off that mount onto its overlay by the
    /// time the workload exits -- so the init holds a descriptor on it across the pivot.
    ///
    /// On macOS the calling binary needs the `com.apple.security.hypervisor` entitlement;
    /// Hypervisor.framework refuses to start a VM without it.
    ///
    /// # A session, and the next one
    ///
    /// The upper layer is the session. A second call over the same one finds everything the
    /// first wrote still in place -- including what it deleted, which overlayfs recorded as
    /// a whiteout. Starting over is deleting that file.
    pub fn enter(&self) -> Result<Infallible> {
        // Checked before a throwaway is formatted, so a mistyped mount does not leave one
        // behind.
        let tags = tags(&self.mounts);
        let points = self
            .mounts
            .iter()
            .map(mount_point)
            .collect::<Result<Vec<_>>>()?;

        let upper = match &self.upper {
            Some(named) => named.clone(),
            None => Upper::new(Upper::SIZE)?,
        };
        let config = self.config.clone().unwrap_or_default();

        // Borrowed by the console device for as long as the VM runs, so they outlive
        // everything below.
        let (stdout, stderr) = (stdout(), stderr());
        #[cfg(unix)]
        let stdin = stdin();

        // The guest kernel, which libkrun `dlopen`s rather than links.
        let mut payload =
            Payload::load_krunfw().map_err(|e| Error::vmm("loading the guest kernel", e))?;

        // What PID 1 does once the guest is up. The init binary and this config are
        // injected into the root through the overlay below, and `apply` is what appends
        // `init=/init.krun` to the kernel cmdline.
        //
        // The two devices go in as environment because that is how libkrun's init is told
        // where its root is -- see `3rdparty/_patches/libkrun-85bed71-overlay-root.patch`.
        let argv = match &self.exec {
            Some(exact) => exact.clone(),
            None => argv(&config, &self.command),
        };
        let env = env(&config, &self.env);
        let mut init = krun_init::Config::builder()
            .args(&argv.iter().map(String::as_str).collect::<Vec<_>>())
            .env(&env.iter().map(String::as_str).collect::<Vec<_>>())
            .workdir(config.working_dir().as_deref().unwrap_or("/"))
            .env_var(&format!("KRUN_BLOCK_LOWER_DEVICE={LOWER_DEVICE}"))
            .env_var(&format!("KRUN_BLOCK_LOWER_FSTYPE={LOWER_FSTYPE}"))
            .env_var(&format!("KRUN_BLOCK_UPPER_DEVICE={UPPER_DEVICE}"))
            .env_var(&format!("KRUN_BLOCK_UPPER_FSTYPE={UPPER_FSTYPE}"));

        // What the guest's `/etc/resolv.conf` names, there being no lease to say it -- see
        // `net::NAMESERVER`.
        if self.network {
            init = init.env_var(&format!("{}={}", net::NAMESERVER_ENV, net::NAMESERVER));
        }

        // One config entry per mount, saying which tag goes where. The devices carrying
        // those tags are attached below, and the init mounts them after the pivot.
        for (point, tag) in points.iter().zip(&tags) {
            init = init.mount(point, MOUNT_FSTYPE, tag);
        }
        let init = init.build();

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

        let mut console = ConsoleDevice::builder();
        // The guest's console on this process's own terminal. The three streams are what
        // differ between hosts, and the only thing here that does -- see the module doc.
        //
        // No stdin on Windows: libkrun's console parks its input thread in a blocking
        // `ReadFile` and the VMM joins that thread on the way out, so a tty stdin would cost
        // the process its exit. stdout is still a tty, so the console keeps its window size.
        #[cfg(unix)]
        let streams = {
            use std::os::fd::AsFd;
            (
                Some(stdin.as_fd()),
                Some(stdout.as_fd()),
                Some(stderr.as_fd()),
            )
        };
        #[cfg(windows)]
        let streams = {
            use std::os::windows::io::AsHandle;
            (None, Some(stdout.as_handle()), Some(stderr.as_handle()))
        };
        console
            .add_default_console(streams.0, streams.1, streams.2)
            .map_err(|e| Error::vmm("wiring the guest console to this terminal", e))?;

        // After the default console and never before: `add_default_console` is what makes
        // port 0 the console the kernel prints on, and port 0 is the first one added.
        if let Some((name, raw)) = &self.port {
            // SAFETY: the descriptors are the caller's and are promised to outlive the
            // machine -- see [`port`](Machine::port). libkrun `dup`s them here, so what is
            // borrowed is borrowed only for the length of this call.
            //
            // One both ways on unix, where a socket carries both directions, and two on
            // Windows, where a console port's reads and writes have to be able to block
            // independently -- see [`Channel`].
            #[cfg(unix)]
            let (input, output) = {
                let fd = unsafe { std::os::fd::BorrowedFd::borrow_raw(*raw) };
                (fd, fd)
            };
            #[cfg(windows)]
            let (input, output) = unsafe {
                use std::os::windows::io::BorrowedHandle;
                (
                    BorrowedHandle::borrow_raw(raw.0 as *mut std::ffi::c_void),
                    BorrowedHandle::borrow_raw(raw.1 as *mut std::ffi::c_void),
                )
            };
            console
                .add_inout_port(name, Some(input), Some(output))
                .map_err(|e| Error::vmm(format!("attaching the {name} console port"), e))?;
        }

        // Read-only at the device and not merely in the guest's mount flags. What is behind
        // this descriptor is the layer files, shared with every other root filesystem over
        // them, so a guest allowed to write to it would be writing into those.
        let disk = self.rootfs.disk();
        let mut lower = BlockDevice::new("vda", named(disk)?, DiskFormat::Vmdk)
            .map_err(|e| Error::vmm(format!("attaching {}", disk.display()), e))?;
        lower.set_read_only(true);

        let upper_device = BlockDevice::new("vdb", named(upper.path())?, DiskFormat::Raw)
            .map_err(|e| Error::vmm(format!("attaching {}", upper.path().display()), e))?;

        let mut devices = MmioDeviceManager::new();
        devices.add(root);
        devices.add(
            console
                .build()
                .map_err(|e| Error::vmm("building the guest console", e))?,
        );
        devices.add(RngDevice::new().map_err(|e| Error::vmm("attaching an entropy source", e))?);
        devices.add(BalloonDevice::new().map_err(|e| Error::vmm("attaching a balloon", e))?);

        // The whole of what a network is here, and the whole of what having none is: the
        // device is attached or it is not -- see [`net`].
        if self.network {
            let published = self
                .published
                .iter()
                .map(|(console, listener)| {
                    listener
                        .try_clone()
                        .map(|listener| (*console, listener))
                        .map_err(|e| {
                            Error::io(format!("sharing the listener for guest port {console}"), e)
                        })
                })
                .collect::<Result<Vec<_>>>()?;
            devices.add(net::attach(published)?);
        }

        // Nothing at all without the feature, which is what `gpu` promises: a build that
        // cannot serve Vulkan runs the session without it rather than refusing it.
        #[cfg(feature = "gpu")]
        if self.gpu {
            devices.add(gpu::device(self.gpu_memory_mib)?);
        }

        // In this order, and the order is the whole of what names them -- see the module
        // doc.
        devices.add(lower);
        devices.add(upper_device);

        // The shares last, since nothing about them depends on where they land. Read-only
        // at the device, which is the whole of what `Mount::read_only` promises.
        for (mount, tag) in self.mounts.iter().zip(&tags) {
            let host = named(&mount.host)?;
            let share = match mount.read_only {
                true => FsDevice::new_read_only(tag, host),
                false => FsDevice::new(tag, host),
            }
            .map_err(|e| Error::vmm(format!("sharing {host} as {tag}"), e))?;
            devices.add(share);
        }

        let vmm = VmmBuilder::new()
            .vcpus(self.vcpus)
            .map_err(|e| Error::vmm(format!("{} vCPUs", self.vcpus), e))?
            .ram_mib(self.ram_mib)
            .map_err(|e| Error::vmm(format!("{} MiB of memory", self.ram_mib), e))?
            .payload(payload)
            .devices(devices)
            .build()
            .map_err(|e| Error::vmm("building the machine", e))?;

        // Here and not a line earlier: building is what opened the block devices, so this
        // is the first moment an upper layer nobody named can lose its name and still be
        // the disk the guest writes to. A named one is untouched.
        upper.discard();

        vmm.run();

        // Reaching this line is a failure, and the one kind that can happen after a machine
        // has started. `run` is an event loop, and the VMM leaves it by `_exit`ing this
        // process with the guest's status -- so it comes back only when the loop itself
        // broke, which libkrun logs and then swallows.
        Err(Error::EventLoop)
    }
}

/// The host's physical memory in bytes, or `None` where it cannot be read.
///
/// What a server holds a machine's size against: a guest's RAM and its GPU's memory are both
/// the host's on a GPU that shares it, so a session asking for more than the two can have
/// together is one this host cannot make.
#[cfg(target_os = "macos")]
pub fn host_memory() -> Option<u64> {
    let mut bytes: u64 = 0;
    let mut len = std::mem::size_of::<u64>();
    // SAFETY: `hw.memsize` is a u64, and `bytes` and `len` describe one.
    let ret = unsafe {
        libc::sysctlbyname(
            c"hw.memsize".as_ptr(),
            (&raw mut bytes).cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    (ret == 0 && bytes > 0).then_some(bytes)
}

/// The host's physical memory in bytes, or `None` where it cannot be read.
#[cfg(all(unix, not(target_os = "macos")))]
pub fn host_memory() -> Option<u64> {
    // SAFETY: sysconf reads a value and touches no memory of ours.
    let (pages, page) = unsafe {
        (
            libc::sysconf(libc::_SC_PHYS_PAGES),
            libc::sysconf(libc::_SC_PAGESIZE),
        )
    };
    (pages > 0 && page > 0).then(|| pages as u64 * page as u64)
}

/// The host's physical memory in bytes, or `None` where it cannot be read.
#[cfg(windows)]
pub fn host_memory() -> Option<u64> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    // SAFETY: an all-zero MEMORYSTATUSEX is valid, and `dwLength` is set before the call
    // reads it.
    let mut status: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    status.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    // SAFETY: `status` is a MEMORYSTATUSEX with its length filled in.
    let ok = unsafe { GlobalMemoryStatusEx(&mut status) };
    (ok != 0 && status.ullTotalPhys > 0).then_some(status.ullTotalPhys)
}

/// libkrun's logger at the level `KRUN_LOG` names (`error`, `warn`, `info`, `debug` or
/// `trace`), on stderr -- which for a boot is its console log. It carries what rutabaga
/// forwards from virglrenderer as well. Without `KRUN_LOG`, only what the guest's init and
/// kernel say comes through. Call it once, before the machine is built: libkrun sets up no
/// logger of its own.
pub fn log_from_env() {
    use krun::{LogLevel, LogOptions, LogStyle};
    let level = match std::env::var("KRUN_LOG").as_deref() {
        Ok("error") => LogLevel::Error,
        Ok("warn") => LogLevel::Warn,
        Ok("info") => LogLevel::Info,
        Ok("debug") => LogLevel::Debug,
        Ok("trace") => LogLevel::Trace,
        _ => {
            if log::set_logger(&GuestConsole).is_ok() {
                log::set_max_level(log::LevelFilter::Error);
            }
            return;
        }
    };
    // A second call finds a logger in place, which is all it would have set up.
    let _ = krun::init_log(None, level, LogStyle::Auto, LogOptions::NO_ENV);
}

/// What is logged when nobody asked for libkrun's own log: the guest's console -- its init
/// and kernel -- which libkrun hands on as records of one target, and nothing else.
struct GuestConsole;

impl GuestConsole {
    const TARGET: &'static str = "init_or_kernel";
    const UNTERMINATED: &'static str = "[missing newline]";
}

impl log::Log for GuestConsole {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.target() == GuestConsole::TARGET
    }

    fn log(&self, record: &log::Record) {
        use std::io::Write as _;
        if !self.enabled(record.metadata()) {
            return;
        }
        let said = record.args().to_string();
        let mut stderr = std::io::stderr().lock();
        let _ = match said.strip_prefix(GuestConsole::UNTERMINATED) {
            Some(part) => stderr.write_all(part.as_bytes()),
            None => writeln!(stderr, "{}", said.strip_suffix('\r').unwrap_or(&said)),
        };
    }

    fn flush(&self) {}
}

/// A session's writable layer: one ext4 image, on the host, holding everything the session
/// writes and nothing the image shipped.
///
/// A disk and not a share, because overlayfs will take nothing else as an upper layer: a
/// virtio-fs share is refused outright, and on a macOS host a share could not hold a
/// whiteout anyway -- `mknod` of the device one is spelled as is `EPERM` for anyone but
/// root. A block device the guest mounts has neither problem.
///
/// Formatted **here** rather than in the guest, because a guest that had to format its own
/// root would need `mkfs.ext4` in the image -- and the point of this is that it works over
/// an image nobody prepared. Empty: the guest's init makes the upperdir and its workdir on
/// it, for a disk it is carrying on from as much as for a new one.
///
/// The size is a **ceiling and not an allocation**. The file is sparse, so a session that
/// writes a kilobyte occupies a kilobyte on the host; what the number bounds is how much a
/// runaway command can write before the guest reports a full disk.
#[derive(Clone, Debug)]
pub struct Upper {
    path: PathBuf,
    /// Whether this one is a throwaway -- made by [`new`](Upper::new) because
    /// nobody named a path, and so nobody's to keep. It is what [`discard`](Upper::discard)
    /// acts on, and what keeps it from ever touching a session a caller means to come back
    /// to.
    scratch: bool,
}

impl Upper {
    /// How large an upper layer may grow, when nothing says otherwise.
    pub const SIZE: u64 = 2 << 30;

    /// Where overlayfs's upperdir is on the filesystem.
    ///
    /// The guest's init makes it, and makes `work/` beside it -- a workdir has to be on the
    /// same filesystem as the upperdir, and this is the only one here that is writable and
    /// outlives the boot. So the filesystem's own root is not the layer: what a session
    /// wrote is one directory down, and `work/` next to it is overlayfs's own and is nobody's
    /// changeset.
    const UPPERDIR: &'static [u8] = b"upper";

    /// The upper layer an earlier session left at `path`, if there is one.
    ///
    /// **Carried on rather than reformatted.** That is what makes a session persist: the
    /// overlay's upperdir is a directory on this disk, so a session booted over the same file
    /// starts holding everything the last one wrote. Deleting the file is how a session is
    /// thrown away, and it is the only way.
    ///
    /// Nothing is made: a path with nothing there is `None`, for the caller to say whether
    /// that is a session to [`create`](Upper::create) or one that has gone.
    pub fn open(path: impl Into<PathBuf>) -> Option<Upper> {
        let path = path.into();
        path.is_file().then_some(Upper {
            path,
            scratch: false,
        })
    }

    /// A new upper layer at `path` seeded with `seed` -- files added, replaced and deleted over
    /// the root filesystem it will be stacked on -- as if a session had already written them.
    ///
    /// **What a session that was handed its own writes back starts from.** `seed` is what
    /// [`to_tree`](Upper::to_tree) read out of some earlier session, and the session booted
    /// over this carries on from where that one stopped.
    ///
    /// An empty `seed` is a session that has written nothing yet.
    ///
    /// **A path that is already there is refused.** One there is carried on with
    /// [`open`](Upper::open), because a session that comes back to the same file is the same
    /// session; writing a seed over a disk that already
    /// holds writes would be two sessions' work in one filesystem with no way to tell which
    /// said what.
    pub fn create(path: impl Into<PathBuf>, size: u64, seed: Tree) -> Result<Upper> {
        let path = path.into();
        let exists = path
            .try_exists()
            .map_err(|e| Error::io(format!("looking for {}", path.display()), e))?;
        if exists {
            return Err(Error::io(
                format!("creating {}", path.display()),
                std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    "an upper layer is what a session starts from, and there is one here \
                     already holding a session's writes",
                ),
            ));
        }
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)
                .map_err(|e| Error::io(format!("creating {}", parent.display()), e))?;
        }

        let mut disk = Tree::new();
        disk.graft(Self::UPPERDIR, seed)?;
        Ext4::from_tree(&disk, &path, size)?;
        Ok(Upper {
            path,
            scratch: false,
        })
    }

    /// Record that the filesystem is no longer mounted, having been flushed.
    ///
    /// **Call this only once the machine is down and was told to flush first.** Nothing
    /// here can check either; the caller is what stopped the machine.
    ///
    /// A Linux kernel clears `s_state`'s "valid" bit when it mounts a filesystem for writing
    /// and sets it again when it unmounts. This one is never unmounted: it is the upper half
    /// of the overlay the guest pivoted onto, so it is the root -- and a root cannot be
    /// unmounted by the system standing on it. The machine is stopped by killing it, and the
    /// bit is still clear afterwards. What makes it safe to say otherwise is the flush: there
    /// is no journal on this filesystem, so once `sync(2)` has returned in the guest and
    /// nothing has run since, what is in the file is the whole of what the guest wrote.
    ///
    /// The other bit is left alone and refused. "Errors detected" is the kernel saying it
    /// found this filesystem inconsistent while it was using it, which no amount of flushing
    /// answers -- and changes read out of one would be wrong in a way nothing downstream could
    /// notice. Settling one already settled is settling it once.
    pub fn settle(&self) -> Result<()> {
        use std::io::{Read, Seek, SeekFrom, Write};

        /// Where the superblock records how the filesystem was left, and the two bits of it
        /// that mean anything here: cleanly unmounted, and found inconsistent while mounted.
        const STATE: u64 = 0x400 + 0x3a;
        const VALID: u16 = 0b01;
        const ERRORS: u16 = 0b10;

        let named = format!("the superblock of {}", self.path.display());
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&self.path)
            .map_err(|e| Error::io(format!("opening {}", self.path.display()), e))?;

        let mut state = [0u8; 2];
        file.seek(SeekFrom::Start(STATE))
            .and_then(|_| file.read_exact(&mut state))
            .map_err(|e| Error::io(format!("reading {named}"), e))?;
        let state = u16::from_le_bytes(state);

        if state & ERRORS != 0 {
            return Err(Error::io(
                format!("reading {named}"),
                std::io::Error::other(
                    "the guest kernel found this filesystem inconsistent while it was running, \
                     so what it holds is not a session's work to keep",
                ),
            ));
        }

        file.seek(SeekFrom::Start(STATE))
            .and_then(|_| file.write_all(&(state | VALID).to_le_bytes()))
            .and_then(|_| file.sync_all())
            .map_err(|e| Error::io(format!("writing {named}"), e))
    }

    /// What the session wrote, as a changeset over the root filesystem it was stacked on:
    /// files added and replaced, and deletions as whiteouts -- what
    /// [`Changeset::from_tree`](crate::filesystem::Changeset::from_tree) writes out as a
    /// layer, and what [`create`](Upper::create) takes back as a seed.
    ///
    /// `skip` names guest paths that are not the session's work, as the guest spells them. A
    /// share is mounted over a directory, and making that directory is a write -- so every
    /// mount point the boot created is recorded here, and a changeset claiming an empty
    /// `/.virtx` would be claiming something nobody asked for. What those paths are is the
    /// caller's to say, since the mounts were.
    ///
    /// **The machine has to be down, and the filesystem [`settle`](Upper::settle)d.** One
    /// that never booted has no upperdir yet, and has written nothing.
    pub fn to_tree(&self, skip: &[&str]) -> Result<Tree> {
        let file = std::fs::File::open(&self.path)
            .map_err(|e| Error::io(format!("opening {}", self.path.display()), e))?;
        let Some(mut changes) = Ext4::new(file).to_tree()?.into_subtree(Self::UPPERDIR) else {
            return Ok(Tree::new());
        };
        for path in skip {
            changes.prune(path.as_bytes());
        }
        Ok(changes)
    }

    /// An upper layer for a session that named none: a fresh ext4 under the host's
    /// temporary directory, which nothing is going to find and nothing is going to carry
    /// on from.
    ///
    /// The name holds the process's id and the moment it was made, so two sessions in one
    /// process do not land on one file. It is a name only because a block device has to be
    /// opened by one, and [`Machine::enter`] removes it once that is done.
    pub fn new(size: u64) -> Result<Upper> {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|since| since.as_nanos())
            .unwrap_or_default();
        let path = std::env::temp_dir().join(format!(
            "virtx-uvm-session-{}-{unique}.ext4",
            std::process::id()
        ));

        Ext4::from_tree(&Tree::new(), &path, size)?;
        Ok(Upper {
            path,
            scratch: true,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Unlink a [`new`](Upper::new) upper layer, and do nothing at all to one a
    /// caller named.
    ///
    /// **Called once the machine has the file open.** A file with no name is still a file
    /// to the descriptor holding it, so the guest goes on writing to a disk that no longer
    /// exists as a path -- and the host reclaims it when that descriptor closes, which
    /// includes every way a process can die without running any code of its own.
    ///
    /// Best effort, and silent: a host that refuses the unlink leaves a file in a temporary
    /// directory, which is the one place a leftover file is already expected.
    fn discard(&self) {
        if self.scratch {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// A host directory, and where the guest sees it.
///
/// A share and not a disk -- the opposite of the [`Upper`]'s reason. What a mount holds is a
/// host directory the host goes on using while the guest reads it, and virtio-fs is what
/// carries it.
///
/// **Read-only is enforced on the host.** [`read_only`](Mount::read_only) attaches the
/// device read-only, which makes the virtiofs server itself refuse every write. That is
/// deliberately not the same as mounting it `ro` in the guest: a guest with a root can
/// remount its own filesystems, and the server is on the other side of the boundary.
///
/// Nothing is created on either side: the host directory has to be there, and
/// [`at`](Mount::at) is made in the guest if it is not -- on the upper layer, so the next
/// session over the same one finds it.
#[derive(Clone, Debug)]
pub struct Mount {
    /// The directory on the host. It has to exist: creating it would be this deciding what
    /// a caller meant by a typo.
    pub host: PathBuf,

    /// Where it appears in the guest, which has to be absolute. Made if it is not there.
    pub at: PathBuf,

    /// Whether the host refuses the guest's writes.
    pub read_only: bool,
}

impl Mount {
    /// Share `host` with the guest, where it appears at `at`. Writable, as a share is unless
    /// something says otherwise.
    pub fn new(host: impl Into<PathBuf>, at: impl Into<PathBuf>) -> Mount {
        Mount {
            host: host.into(),
            at: at.into(),
            read_only: false,
        }
    }

    /// Attach it read-only, so that the host refuses every write the guest makes to it.
    pub fn read_only(mut self) -> Mount {
        self.read_only = true;
        self
    }
}

/// The tag each mount is attached under, one per mount and in the mounts' own order.
///
/// Position and nothing else, because a tag has no meaning beyond joining the device the
/// host attached to the entry the guest mounts, and a name made from the host path would
/// only make two mounts of one directory collide.
fn tags(mounts: &[Mount]) -> Vec<String> {
    (0..mounts.len())
        .map(|i| format!("{MOUNT_TAG}{i}"))
        .collect()
}

/// Where a mount goes in the guest, refused unless it is absolute.
///
/// Absolute *in the guest*, which is a leading `/` and is asked of the text rather than of
/// `Path`: `Path::is_absolute` answers by the rules of whichever host this is, and on
/// Windows `/data` is not one.
fn mount_point(mount: &Mount) -> Result<&str> {
    let at = named(&mount.at)?;
    match at.starts_with('/') {
        true => Ok(at),
        false => Err(Error::MountPoint { at: at.to_string() }),
    }
}

/// A path as libkrun takes it, which is a `&str`.
///
/// Refused rather than lossily converted: what `Path::display` hands back for a byte that
/// was not text is `U+FFFD`, and a name with one of those in it opens some other file or
/// none.
fn named(path: &Path) -> Result<&str> {
    path.to_str().ok_or_else(|| {
        Error::io(
            format!(
                "{} is not valid UTF-8, which libkrun cannot name",
                path.display()
            ),
            std::io::Error::from(std::io::ErrorKind::InvalidInput),
        )
    })
}

/// What the guest runs: the image's entrypoint, then whatever stands for its command.
///
/// Docker's rule, because these are docker's fields: asking for a command replaces the
/// image's `Cmd` and leaves its `Entrypoint` in front. An image that states neither runs a
/// shell, which is the only thing left that is certain to be there.
fn argv(config: &Config, asked: &[String]) -> Vec<String> {
    let mut argv = config.entrypoint().clone().unwrap_or_default();
    match asked.is_empty() {
        true => argv.extend(config.cmd().clone().unwrap_or_default()),
        false => argv.extend_from_slice(asked),
    }
    if argv.is_empty() {
        argv.push("/bin/sh".to_string());
    }
    argv
}

/// The environment the guest runs in: the image's, then the session's on top.
///
/// `PATH` and `HOME` are added when nothing set them, with the values docker uses. An image
/// that states neither is not unusual, and a session that cannot find `ls` is not a useful
/// one.
fn env(config: &Config, extra: &[String]) -> Vec<String> {
    let mut env = config.env().clone().unwrap_or_default();
    env.extend_from_slice(extra);

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

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Scratch {
            let path = std::env::temp_dir().join(format!(
                "virtx-uvm-lib-session-{name}-{}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            Scratch(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A root filesystem, made of one empty layer -- a session's builder asks nothing of it
    /// but that it is one.
    fn rootfs(scratch: &Scratch) -> Rootfs {
        std::fs::create_dir_all(&scratch.0).unwrap();
        let layer = scratch.0.join("layer.erofs");
        filesystem::Erofs::from_tree(&Tree::new(), std::fs::File::create(&layer).unwrap()).unwrap();
        Rootfs::create(&[layer], &scratch.0.join("rootfs")).unwrap()
    }

    fn config(entrypoint: &[&str], cmd: &[&str]) -> Config {
        let some = |v: &[&str]| match v.is_empty() {
            true => None,
            false => Some(v.iter().map(|s| s.to_string()).collect()),
        };
        let mut config = Config::default();
        config.set_entrypoint(some(entrypoint));
        config.set_cmd(some(cmd));
        config
    }

    fn asked(argv: &[&str]) -> Vec<String> {
        argv.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn what_is_not_named_keeps_its_default() {
        let scratch = Scratch::new("defaults");
        let session = Machine::new(rootfs(&scratch), None);
        assert_eq!(session.vcpus, 2);
        assert_eq!(session.ram_mib, 2048);
        assert!(session.upper.is_none());
        assert!(session.command.is_empty());
        assert!(session.exec.is_none());
        assert!(session.env.is_empty());
        assert!(
            !session.network,
            "a session that said nothing got a network"
        );
        assert!(session.published.is_empty());
        assert!(!session.gpu);
        assert!(session.gpu_memory_mib.is_none());
    }

    /// A command is one argv, so the last word on it wins; an environment is a list, so
    /// every word on it is kept.
    #[test]
    fn a_command_replaces_and_an_environment_appends() {
        let scratch = Scratch::new("command");
        let session = Machine::new(rootfs(&scratch), None)
            .command(["sh", "-c", "true"])
            .command(["uname", "-a"])
            .env(["LANG=C"])
            .env_var("TZ=UTC");

        assert_eq!(session.command, ["uname", "-a"]);
        assert_eq!(session.env, ["LANG=C", "TZ=UTC"]);
    }

    #[test]
    fn mounts_append_in_the_order_they_are_given() {
        let scratch = Scratch::new("mounts");
        let session = Machine::new(rootfs(&scratch), None)
            .mount(Mount::new("/host/a", "/a"))
            .mounts([Mount::new("/host/b", "/b").read_only()]);

        let at: Vec<_> = session.mounts.iter().map(|m| m.at.as_path()).collect();
        assert_eq!(at, [Path::new("/a"), Path::new("/b")]);
        assert!(!session.mounts[0].read_only);
        assert!(session.mounts[1].read_only);
    }

    /// Caught before anything is formatted or booted, where there is a caller to tell.
    #[test]
    fn a_mount_point_that_is_not_absolute_is_refused() {
        let scratch = Scratch::new("relative");
        let session = Machine::new(rootfs(&scratch), None).mount(Mount::new("/host", "data"));
        assert!(matches!(session.enter(), Err(Error::MountPoint { .. })));
    }

    /// One tag per mount, and the mounts' own order, since that is what pairs a device with
    /// the config entry that mounts it.
    #[test]
    fn every_mount_gets_its_own_tag() {
        let mounts = vec![
            Mount::new("/host/a", "/a"),
            Mount::new("/host/b", "/b"),
            // The same directory twice, which two names is the whole point of.
            Mount::new("/host/a", "/c"),
        ];
        assert_eq!(tags(&mounts), ["mnt0", "mnt1", "mnt2"]);
        assert!(tags(&[]).is_empty());
    }

    /// An upper layer is formatted once, and opened as it is after that.
    #[test]
    fn an_upper_layer_that_is_there_is_carried_on() {
        let scratch = Scratch::new("upper");
        let path = scratch.0.join("deep").join("session.ext4");
        assert!(Upper::open(&path).is_none(), "nothing there was opened");

        let made = Upper::create(&path, 64 << 20, Tree::new()).unwrap();
        assert_eq!(made.path(), path);
        assert!(!made.scratch);
        Ext4::new(std::fs::File::open(&path).unwrap())
            .to_tree()
            .unwrap();

        std::fs::write(&path, b"what the last session left").unwrap();
        let opened = Upper::open(&path).unwrap();
        assert!(!opened.scratch);
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"what the last session left",
            "an upper layer that was there was formatted again"
        );
    }

    fn meta() -> filesystem::Meta {
        filesystem::Meta {
            mode: 0o644,
            ..filesystem::Meta::default()
        }
    }

    /// Every path in `tree`, and what each file holds -- what a changeset says, without the
    /// inode numbers two reads of one need not agree on.
    fn said(tree: &Tree) -> Vec<(String, String)> {
        tree.walk()
            .into_iter()
            .map(|(path, _, entry)| {
                let what = match entry {
                    filesystem::Entry::File { data, .. } => {
                        String::from_utf8_lossy(&data.contents().unwrap()).into_owned()
                    }
                    filesystem::Entry::Directory { opaque: true, .. } => "opaque".into(),
                    filesystem::Entry::Directory { .. } => "dir".into(),
                    filesystem::Entry::Whiteout => "whiteout".into(),
                    other => format!("{other:?}"),
                };
                (String::from_utf8(path).unwrap(), what)
            })
            .collect()
    }

    /// What a session is handed back is what it reads out: files, deletions and a directory
    /// that hides what is below it, each as itself.
    #[test]
    fn changes_come_out_as_they_went_in() {
        let scratch = Scratch::new("changes");
        let mut changes = Tree::new();
        changes
            .insert(
                b"etc/hosts",
                filesystem::Entry::File {
                    meta: meta(),
                    data: filesystem::Data::Buffer(b"127.0.0.1 here".to_vec()),
                },
            )
            .unwrap();
        changes
            .insert(b"etc/motd", filesystem::Entry::Whiteout)
            .unwrap();
        changes.opaque(b"var/cache").unwrap();
        let expected = said(&changes);

        let upper = Upper::create(scratch.0.join("seeded.ext4"), 64 << 20, changes).unwrap();
        let read = upper.to_tree(&[]).unwrap();
        assert_eq!(said(&read), expected);
    }

    /// A mount point the boot made is not the session's work.
    #[test]
    fn what_is_skipped_is_not_in_the_changes() {
        let scratch = Scratch::new("skip");
        let mut changes = Tree::new();
        changes.opaque(b".virtx").unwrap();
        changes.opaque(b"work").unwrap();
        let upper = Upper::create(scratch.0.join("skip.ext4"), 64 << 20, changes).unwrap();

        let read = upper.to_tree(&["/.virtx"]).unwrap();
        let paths: Vec<_> = said(&read).into_iter().map(|(path, _)| path).collect();
        assert_eq!(paths, ["", "work"]);
    }

    /// One that never booted has no upperdir, and has written nothing.
    #[test]
    fn a_fresh_upper_layer_has_no_changes() {
        let scratch = Scratch::new("fresh");
        let upper = Upper::create(scratch.0.join("fresh.ext4"), 64 << 20, Tree::new()).unwrap();
        assert_eq!(
            said(&upper.to_tree(&[]).unwrap()),
            [("".into(), "dir".into())]
        );
    }

    #[test]
    fn creating_over_an_upper_layer_is_refused() {
        let scratch = Scratch::new("over");
        let path = scratch.0.join("taken.ext4");
        Upper::create(&path, 64 << 20, Tree::new()).unwrap();
        assert!(matches!(
            Upper::create(&path, 64 << 20, Tree::new()),
            Err(Error::Io { .. })
        ));
    }

    /// Left mounted, as a killed guest leaves it, it is not read until it is settled; one
    /// the kernel found broken is never settled.
    #[test]
    fn settling_says_what_the_flush_made_true() {
        use std::io::{Read, Seek, SeekFrom, Write};
        const STATE: u64 = 0x400 + 0x3a;
        let set = |path: &Path, state: u16| {
            let mut file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
            file.seek(SeekFrom::Start(STATE)).unwrap();
            file.write_all(&state.to_le_bytes()).unwrap();
        };
        let get = |path: &Path| {
            let mut file = std::fs::File::open(path).unwrap();
            let mut state = [0u8; 2];
            file.seek(SeekFrom::Start(STATE)).unwrap();
            file.read_exact(&mut state).unwrap();
            u16::from_le_bytes(state)
        };

        let scratch = Scratch::new("settle");
        let upper = Upper::create(scratch.0.join("settle.ext4"), 64 << 20, Tree::new()).unwrap();

        set(upper.path(), 0);
        assert!(upper.to_tree(&[]).is_err(), "one left mounted was read");
        upper.settle().unwrap();
        assert_eq!(get(upper.path()) & 1, 1, "the valid bit is not set");
        upper.settle().unwrap();
        upper.to_tree(&[]).unwrap();

        set(upper.path(), 0b10);
        assert!(
            upper.settle().is_err(),
            "a filesystem found broken was settled"
        );
    }

    #[test]
    fn this_host_has_memory() {
        assert!(host_memory().is_some_and(|bytes| bytes > 0));
    }

    /// A throwaway goes when it is discarded; one a caller named never does.
    #[test]
    fn only_a_throwaway_is_discarded() {
        let scratch = Scratch::new("discard");
        let named = Upper::create(scratch.0.join("kept.ext4"), 64 << 20, Tree::new()).unwrap();
        named.discard();
        assert!(named.path().exists());

        let throwaway = Upper::new(64 << 20).unwrap();
        assert!(throwaway.path().exists());
        throwaway.discard();
        assert!(!throwaway.path().exists());
    }

    #[test]
    fn an_image_that_states_a_command_runs_it() {
        assert_eq!(argv(&config(&[], &["nginx", "-g"]), &[]), ["nginx", "-g"]);
        assert_eq!(
            argv(&config(&["/tini", "--"], &["nginx"]), &[]),
            ["/tini", "--", "nginx"]
        );
    }

    /// Docker's rule: what is asked for replaces the `Cmd` and keeps the `Entrypoint`.
    #[test]
    fn asking_for_a_command_replaces_the_cmd_and_not_the_entrypoint() {
        assert_eq!(
            argv(&config(&["/tini", "--"], &["nginx"]), &asked(&["sh"])),
            ["/tini", "--", "sh"]
        );
        assert_eq!(
            argv(&config(&[], &["nginx"]), &asked(&["sh", "-c", "hi"])),
            ["sh", "-c", "hi"]
        );
    }

    #[test]
    fn an_image_that_states_nothing_runs_a_shell() {
        assert_eq!(argv(&config(&[], &[]), &[]), ["/bin/sh"]);
    }

    #[test]
    fn a_path_the_image_did_not_set_is_added_and_one_it_did_is_left_alone() {
        let mut stated = Config::default();
        stated.set_env(Some(vec!["PATH=/opt/bin".to_string()]));
        let stated = env(&stated, &[]);
        assert!(stated.contains(&"PATH=/opt/bin".to_string()), "{stated:?}");
        assert!(!stated.iter().any(|var| var == DEFAULT_PATH), "{stated:?}");

        let silent = env(&Config::default(), &[]);
        assert!(silent.iter().any(|var| var == DEFAULT_PATH), "{silent:?}");
        assert!(silent.contains(&"HOME=/root".to_string()), "{silent:?}");
    }

    /// Later wins in the environment a process is handed, so the session's copy of a name
    /// the image set is the one that takes effect.
    #[test]
    fn the_sessions_environment_goes_after_the_images() {
        let mut stated = Config::default();
        stated.set_env(Some(vec!["LANG=C".to_string()]));
        let env = env(&stated, &asked(&["LANG=en_US.UTF-8"]));
        let at = |name: &str| env.iter().position(|var| var == name);
        assert!(at("LANG=C") < at("LANG=en_US.UTF-8"), "{env:?}");
    }
}
