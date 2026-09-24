//! A session, and what running one takes.
//!
//! # What it takes
//!
//! Three things, and only one of them is made here:
//!
//! ```text
//!   the cache   ──► provision   ──► <stack>.vmdk   the image, read-only, shared
//!   the rootfs  ──► Ext4        ──► session.ext4   this session's, written to
//!                                        │
//!                                        ▼
//!                                     a machine, which does not come back
//! ```
//!
//! The disk is [`provision`]'s and is made once per image, whatever asks for it. The
//! rootfs is an [`Ext4`] the caller hands over, made once per session and made by whoever
//! wants to keep it -- a session given none makes itself a throwaway. And the stacking of
//! the two is neither end's: it happens in the guest, where overlayfs is.
//!
//! # The devices, and what each is for
//!
//! ```text
//! virtio-fs  /dev/root   an empty root, holding the init and the directories it mounts
//!                        over -- replaced by the overlay before anything else runs
//! virtio-blk /dev/vda    the image's disk, read-only -- the overlay's lower
//! virtio-blk /dev/vdb    this session's ext4 -- the overlay's upper layer
//! virtio-con hvc0        the guest's console, on this process's own stdin and stdout
//! virtio-net eth0        the guest's network, on a session that asked for one -- see below
//! virtio-fs  mnt0, mnt1  one per [`Mount`], mounted by tag on the root after the pivot
//! ```
//!
//! The mounts go on last, after both disks, because what names a disk is the order the
//! disks went on and a share takes no name from it. What tells the guest to mount them is
//! the init's config rather than a device: the host says `mnt0` goes at `/data`, and the
//! init mounts it there once the overlay is the root -- see
//! `3rdparty/_patches/libkrun-85bed71-session-mounts.patch`.
//!
//! **Attach order is what fixes the two disk names**, and the guest is told which is
//! which by name: the lower goes on first and is `/dev/vda`, the upper layer second and
//! `/dev/vdb`. The pair of constants below and the order [`Session::enter`] adds them in
//! are one statement made in two places, which is why they are next to each other.
//!
//! # The network, and where it actually is
//!
//! On a thread of this process, behind a socketpair: the device puts the guest's ethernet
//! frames down one end and [`net`](super::net) is what reads them. So a session's network
//! is not a thing the host has and the guest is let onto -- it is made, per session, out
//! of host sockets, and what a level like [`Network::Public`] means is which of those
//! sockets ever get opened.
//!
//! The guest configures itself over DHCP before the workload starts, which is how an
//! image that was never prepared for any of this gets an address, a route and a resolver.
//! A session that asked for no network gets no device, and there is nothing in the guest
//! that can put one back. See [`Session::network`].
//!
//! # The console is the one thing that differs per host
//!
//! libkrun's public API is `std::os::fd` everywhere but Windows, where a console stream
//! is a `BorrowedHandle` rather than a `BorrowedFd`. That one call is `cfg`'d and nothing
//! else here is: a session is the same machine on every host this builds for.
//!
//! # The root that is thrown away
//!
//! The virtio-fs root is [`FsDevice::new_null`] -- a filesystem with nothing in it but
//! what the overlay puts there: the init binary, its config, and the empty directories
//! the init mounts the two disks over. Nothing on the host backs it. It exists for the
//! moment between the kernel handing over and the init pivoting onto the overlay, and
//! after the pivot it is not reachable by any path.

use std::convert::Infallible;
use std::fmt::Display;
use std::io::{stderr, stdout};
use std::path::Path;

// The one thing here that is not the same on every host -- see the module doc.
#[cfg(unix)]
use std::io::stdin;

use krun::{
    BalloonDevice, BlockDevice, ConsoleDevice, DiskFormat, FsDevice, FsOverlay, MmioDeviceManager,
    Payload, RngDevice, VmmBuilder,
};

use crate::image::{Cache, ImageState, provision};

use super::{Error, Ext4, Mount, Network, Result, net};

/// The tag the guest kernel mounts as `/`, which libkrun's default kernel cmdline names
/// with `rootfstype=virtiofs`.
const ROOT_TAG: &str = "/dev/root";

/// Where the two disks land in the guest, in the order [`Session::enter`] attaches them.
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

/// What the guest mounts a [`Mount`] as, which is the only thing a virtio-fs share can
/// be mounted as.
const MOUNT_FSTYPE: &str = "virtiofs";

/// `S_IFDIR | 0755`, as [`FsOverlay::add_dir`] wants a mode.
const DIR_MODE: u32 = 0o040_755;

/// What a `PATH`-less image gets, which is what docker gives one.
const DEFAULT_PATH: &str = "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// What a session is, beyond the image it runs: where its writes go, how big a machine
/// runs them, and what it runs.
///
/// Every field has a default but one: the image, which nothing can stand in for and which
/// [`image`](Session::image) is the only way to say. The rootfs has one -- a session
/// given none gets a layer it does not keep. Anything else is changed by naming it, and
/// [`enter`](Session::enter) is what turns the whole of it into a machine:
///
/// ```no_run
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// use cortex_krun::session::{Ext4, Session};
///
/// Session::new()
///     .image("alpine:3.21")
///     .rootfs(Ext4::at("/tmp/demo.ext4", Ext4::SIZE)?)
///     .ram_mib(4096)
///     .command(["/bin/sh", "-c", "echo hi"])
///     .enter()?;
/// # Ok(())
/// # }
/// ```
///
/// **The rootfs comes in already made.** It is the one part of a session that is a
/// file on the host rather than a word about one, and [`Ext4`] is where every
/// question about it is answered -- where it goes, how large it may grow, whether it is
/// one to keep. What is left here describes a machine, and nothing here touches the host
/// until [`enter`](Session::enter) is called.
#[derive(Clone, Debug)]
pub struct Session {
    image: Option<String>,
    cache: Option<Cache>,
    rootfs: Option<Ext4>,
    mounts: Vec<Mount>,
    vcpus: u8,
    ram_mib: u32,
    network: Network,
    host_ports: Vec<u16>,
    gpu: bool,
    gpu_memory_mib: Option<u32>,
    /// A named virtio-console port, and the descriptor behind it -- see
    /// [`port`](Session::port). Held as a number rather than a borrow so that a
    /// [`Session`] stays a plain value; keeping the descriptor open is the caller's.
    port: Option<(String, i32)>,
    command: Vec<String>,
    /// The whole argv, for a caller who is not running the image's program at all -- see
    /// [`exec`](Session::exec). When set it is what runs, and `command` is not consulted.
    exec: Option<Vec<String>>,
    env: Vec<String>,
}

impl Default for Session {
    fn default() -> Session {
        Session::new()
    }
}

impl Session {
    /// The guest's RAM when [`ram_mib`](Session::ram_mib) is not said.
    pub const DEFAULT_RAM_MIB: u32 = 2048;

    /// A default machine, over a rootfs nobody keeps -- and over no image, which
    /// [`image`](Session::image) has to say before [`enter`](Session::enter) will start
    /// anything.
    pub fn new() -> Session {
        Session {
            image: None,
            cache: None,
            rootfs: None,
            mounts: Vec::new(),
            // Two, because a command and whatever is watching it are two things wanting
            // the processor at once, and one vCPU turns that into a queue.
            vcpus: 2,
            // Enough for a package install, which is the first thing anyone does.
            ram_mib: Self::DEFAULT_RAM_MIB,
            // What a session that said nothing about it gets, and the only level that
            // needs nothing of the host -- see `Session::network`.
            network: Network::Disabled,
            host_ports: Vec::new(),
            gpu: false,
            gpu_memory_mib: None,
            port: None,
            command: Vec::new(),
            exec: None,
            env: Vec::new(),
        }
    }

    /// What this session runs, named the way an OCI reference is written --
    /// `alpine:3.21`, `ghcr.io/owner/repo:tag`, `repo@sha256:...`. A
    /// [`Reference`](crate::image::Reference) is taken too, since what is kept is its
    /// canonical spelling either way.
    ///
    /// **A session has no default image**, so leaving this unsaid is what
    /// [`enter`](Session::enter) refuses with [`Error::NoImage`]. The name is not parsed
    /// here and the cache is not asked about it: a reference that is not one, or one
    /// nothing pulled, is [`enter`](Session::enter)'s to report, which is where every
    /// other failure to get a machine started already arrives.
    ///
    /// Replaces what an earlier call said, a session being one image.
    pub fn image(mut self, image: impl Display) -> Session {
        self.image = Some(image.to_string());
        self
    }

    /// Where the image is looked for, which is the same default
    /// [`pull`](crate::image::pull) writes to when nothing says otherwise.
    pub fn cache(mut self, cache: Cache) -> Session {
        self.cache = Some(cache);
        self
    }

    /// The ext4 image this session writes to, which [`Ext4::at`] made or found.
    /// Keeping the file is what carries a session on; deleting it is what throws one
    /// away.
    ///
    /// **Left unsaid, the session is a throwaway**: [`enter`](Session::enter) makes an
    /// [`Ext4::scratch`] under the host's temporary directory and unlinks it the
    /// moment the machine has it open, so the writes go somewhere real and nothing
    /// outlives the process. A throwaway of some other size is that call made by hand and
    /// passed in here.
    pub fn rootfs(mut self, rootfs: Ext4) -> Session {
        self.rootfs = Some(rootfs);
        self
    }

    /// A host directory the guest can see, at the path the [`Mount`] names. Appends, so
    /// several calls are several mounts.
    ///
    /// The share is attached to the machine and the guest is told to mount it once its
    /// root is the overlay -- so a mount lands on what the workload sees, and the
    /// directory it goes on is made on the upper layer if the image shipped none.
    pub fn mount(mut self, mount: Mount) -> Session {
        self.mounts.push(mount);
        self
    }

    /// Several [`Mount`]s at once, in the order given. Appends, the way
    /// [`mount`](Session::mount) does.
    pub fn mounts(mut self, mounts: impl IntoIterator<Item = Mount>) -> Session {
        self.mounts.extend(mounts);
        self
    }

    pub fn vcpus(mut self, vcpus: u8) -> Session {
        self.vcpus = vcpus;
        self
    }

    pub fn ram_mib(mut self, ram_mib: u32) -> Session {
        self.ram_mib = ram_mib;
        self
    }

    /// How much of the network the guest can reach, which by default is none of it.
    ///
    /// **`Disabled` is an isolation nothing in the guest can undo**, because what it
    /// leaves out is a device. There is no interface to bring up, no route to add and no
    /// address to claim -- a socket the guest opens to anywhere but itself has nowhere to
    /// go. It is the default because it is the only level that asks nothing of the host,
    /// and because a session that did not say it wanted to reach the network should not.
    ///
    /// Every other level is a device, and with it an address, a default route, an MTU and
    /// a `/etc/resolv.conf`, all of which arrive over DHCP before the workload starts. An
    /// image needs nothing in it for that: libkrun's init is the client, and
    /// [`session::net`](super::net) is the server.
    ///
    /// | level | what it reaches |
    /// |---|---|
    /// | [`Disabled`](Network::Disabled) | nothing, there being no device |
    /// | [`HostOnly`](Network::HostOnly) | names, and the ports [`host_ports`](Session::host_ports) granted |
    /// | [`Public`](Network::Public) | the internet, and the grant -- not this machine, not its LAN |
    /// | [`Full`](Network::Full) | every address, and the grant |
    ///
    /// **Reaching this machine is never a level.** It is
    /// [`host_ports`](Session::host_ports) at all four, which is what keeps widening a
    /// session's reach outward from widening it inward by accident.
    pub fn network(mut self, network: Network) -> Session {
        self.network = network;
        self
    }

    /// TCP ports on this host the guest may connect to, at the gateway address its
    /// default route already names. Appends, so several calls are several ports.
    ///
    /// Opened on loopback and nowhere else: a granted port is a service on *this*
    /// machine, and one resolved through an interface would be whatever answers that port
    /// on whichever network the host is attached to.
    ///
    /// Granted at every [`network`](Session::network) level but
    /// [`Disabled`](Network::Disabled), which has no device to carry it -- see that
    /// method on why the host is a grant rather than a level.
    pub fn host_ports(mut self, ports: impl IntoIterator<Item = u16>) -> Session {
        self.host_ports.extend(ports);
        self
    }

    /// Give the guest a virtio-gpu device carrying Vulkan, by way of
    /// [venus](https://docs.mesa3d.org/drivers/venus.html).
    ///
    /// **Ignored unless the crate was built with the `gpu` feature**, which is what pulls
    /// in virglrenderer and libkrun's GPU support. Without it this is a flag that changes
    /// nothing, so a caller need not `cfg` around it -- a build that cannot serve Vulkan
    /// runs the same session without it rather than refusing to start.
    ///
    /// The image has to carry the guest half either way. On Alpine that is `apk add
    /// mesa-vulkan-virtio vulkan-loader`; an image without it boots and finds no device.
    pub fn gpu(mut self, gpu: bool) -> Session {
        self.gpu = gpu;
        self
    }

    /// How much memory the guest's GPU may hold, in mebibytes: the host-visible window its
    /// Vulkan memory is mapped through, and the heap size the device reports to the guest --
    /// see [`gpu`'s module doc](super::gpu). `None` is the 8 GiB libkrun gives.
    ///
    /// Beside [`ram_mib`](Session::ram_mib) and not out of it: on a GPU that shares the
    /// host's memory, all of it can be host memory in use at once. Ignored without
    /// [`gpu`](Session::gpu).
    pub fn gpu_memory_mib(mut self, gpu_memory_mib: u32) -> Session {
        self.gpu_memory_mib = Some(gpu_memory_mib);
        self
    }

    /// A named virtio-console port beside the guest's console, carrying whatever `fd` is
    /// the host end of.
    ///
    /// The console itself is this process's terminal and is the kernel's: it is where a
    /// panic and every `printk` land, so a protocol on it is one a single kernel warning
    /// would corrupt. A port is a second channel on the same device, found in the guest by
    /// the name given here -- `/dev/virtio-ports/<name>` where the guest has udev rules,
    /// and the `name` attribute under `/sys/class/virtio-ports` where it does not.
    ///
    /// **The same descriptor both ways.** A socket is bidirectional and the port the guest
    /// opens is one thing rather than a pair, so `fd` is used for input and output alike.
    ///
    /// `fd` is borrowed and not taken: it has to stay open for as long as the machine runs,
    /// which is the rest of the process -- see [`enter`](Session::enter). Replaces what an
    /// earlier call said.
    ///
    /// **A port is written to a queue nothing is reading until the guest opens it**, and
    /// the driver discards what arrives before then rather than queueing it. So a caller
    /// on the other end of `fd` cannot speak first: something in the guest has to say it
    /// is there.
    #[cfg(unix)]
    pub fn port(mut self, name: impl Into<String>, fd: std::os::fd::RawFd) -> Session {
        self.port = Some((name.into(), fd));
        self
    }

    /// What to run, in place of what the image states as its `Cmd`.
    ///
    /// **The image's `Entrypoint` still goes in front of it**, which is what docker does
    /// with the same words: an image whose entrypoint is `/usr/bin/tini --` runs its
    /// entrypoint whatever it is asked for. Left unsaid is the image's own `Cmd`, and an
    /// image that states neither runs `/bin/sh`.
    ///
    /// Replaces what an earlier call said rather than appending to it, since a command is
    /// one argv and not a list of them.
    pub fn command<S: Into<String>>(mut self, command: impl IntoIterator<Item = S>) -> Session {
        self.command = command.into_iter().map(Into::into).collect();
        self
    }

    /// Run exactly this, in place of the image's `Entrypoint` **and** its `Cmd`.
    ///
    /// The difference from [`command`](Session::command) is the entrypoint, and it is the
    /// whole difference: `command` is docker's rule -- an image whose entrypoint is
    /// `/usr/bin/tini --` runs its entrypoint whatever it is asked for -- and this is for a
    /// caller running something that is not the image's program at all. An agent put into
    /// the guest over a share is the case it exists for: an image stating
    /// `ENTRYPOINT ["python"]` would otherwise run it as python's argument.
    ///
    /// The image's environment and working directory still apply. What is replaced is what
    /// runs, not the root it runs on.
    ///
    /// Replaces what an earlier call said, and takes precedence over
    /// [`command`](Session::command) whichever order the two were called in.
    pub fn exec<S: Into<String>>(mut self, argv: impl IntoIterator<Item = S>) -> Session {
        self.exec = Some(argv.into_iter().map(Into::into).collect());
        self
    }

    /// Environment on top of the image's, each `KEY=value`. A name the image already set
    /// is set twice and the later one wins, so this overrides rather than merges.
    ///
    /// Appends, so several calls and one call of several are the same thing.
    pub fn env<S: Into<String>>(mut self, env: impl IntoIterator<Item = S>) -> Session {
        self.env.extend(env.into_iter().map(Into::into));
        self
    }

    /// One `KEY=value`, for the caller with one to add.
    pub fn env_var(mut self, var: impl Into<String>) -> Session {
        self.env.push(var.into());
        self
    }

    /// Boot the [`image`](Session::image) with this session's writable layer over it, run
    /// what it says, and become the machine that does.
    ///
    /// The image has to be named and it has to be in the cache already, exactly as
    /// [`provision`] wants it -- this asks no registry anything.
    ///
    /// **This does not return.** It is not that the machine runs forever -- a session that
    /// was asked for `echo hi` prints it and is over in seconds. It is that the way it
    /// ends is *this process* ending: the VMM exits with the guest's own status, so a
    /// session asked for `exit 42` leaves the shell a 42 and everything a caller writes
    /// after the call is unreachable. What comes back is only ever a failure to get that
    /// far, which is what the [`Infallible`] says.
    ///
    /// That the status arrives at all takes the patch below. The guest reports it as an
    /// ioctl on the virtiofs mount, and a session has pivoted off that mount onto its
    /// overlay by the time the workload exits -- so the init holds a descriptor on it
    /// across the pivot. Without that every session exits 0, whatever it ran.
    ///
    /// On macOS the calling binary needs the `com.apple.security.hypervisor` entitlement;
    /// `scripts/macos-run.sh`, registered as the cargo runner, ad-hoc signs it for `cargo
    /// run`. Hypervisor.framework refuses to start a VM without it.
    ///
    /// # A session, and the next one
    ///
    /// The rootfs is the session, and [`Ext4::at`] has already made it or found it by
    /// the time this is called. A second call over the same one finds
    /// everything the first wrote still in place -- including what it deleted, which
    /// overlayfs recorded as a whiteout. Starting over is deleting that file.
    ///
    /// A session given no rootfs is the same machine over a file that is unlinked as
    /// soon as the machine holds it, so there is no second call to make.
    ///
    /// ```no_run
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// use cortex_krun::session::Session;
    ///
    /// Session::new()
    ///     .image("alpine:3.21")
    ///     .command(["/bin/sh", "-c", "uname -a"])
    ///     .enter()?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn enter(&self) -> Result<Infallible> {
        // The image first. It is the one step that can fail on something the caller can
        // still do something about -- an image nobody named, or nobody pulled -- and doing
        // it before a throwaway is formatted means a mistyped reference does not leave one
        // behind. A named rootfs is the caller's and was made before this was called.
        let image = self.image.as_deref().ok_or(Error::NoImage)?;
        let disk = provision(image, self.cache.clone())?;
        let rootfs = match &self.rootfs {
            Some(named) => named.clone(),
            None => Ext4::scratch(Ext4::SIZE)?,
        };

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
        // where its root is -- see `3rdparty/_patches/libkrun-85bed71-overlay-root.patch`. They
        // reach the workload's environment too, the way `KRUN_BLOCK_ROOT_*` already does.
        let argv = match &self.exec {
            Some(exact) => exact.clone(),
            None => argv(&disk.config, &self.command),
        };
        let env = env(&disk.config, &self.env);
        let mut init = krun_init::Config::builder()
            .args(&argv.iter().map(String::as_str).collect::<Vec<_>>())
            .env(&env.iter().map(String::as_str).collect::<Vec<_>>())
            .workdir(disk.config.working_dir.as_deref().unwrap_or("/"))
            .env_var(&format!("KRUN_BLOCK_LOWER_DEVICE={LOWER_DEVICE}"))
            .env_var(&format!("KRUN_BLOCK_LOWER_FSTYPE={LOWER_FSTYPE}"))
            .env_var(&format!("KRUN_BLOCK_UPPER_DEVICE={UPPER_DEVICE}"))
            .env_var(&format!("KRUN_BLOCK_UPPER_FSTYPE={UPPER_FSTYPE}"));

        // What makes libkrun's init run a DHCP client on `eth0` -- and so what gets the
        // guest its address, its route and its `/etc/resolv.conf`, none of which an image
        // has anything in it to set up. The server answering is `net`.
        if self.network.attached() {
            init = init.env_var(&format!("{}=1", net::DHCP_ENV));
        }

        // One config entry per mount, saying which tag goes where. The devices carrying
        // those tags are attached below, and the init mounts them after the pivot.
        let tags = tags(&self.mounts);
        for (mount, tag) in self.mounts.iter().zip(&tags) {
            init = init.mount(mount_point(mount)?, MOUNT_FSTYPE, tag);
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
        // `ReadFile` and the VMM joins that thread on the way out, so a tty stdin would
        // cost the process its exit. stdout is still a tty, so the console keeps its
        // window size.
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
        #[cfg(unix)]
        if let Some((name, fd)) = &self.port {
            use std::os::fd::BorrowedFd;

            // SAFETY: the descriptor is the caller's and is promised to outlive the machine
            // -- see [`port`]. libkrun `dup`s it here, so what is borrowed is borrowed only
            // for the length of this call.
            let fd = unsafe { BorrowedFd::borrow_raw(*fd) };
            console
                .add_inout_port(name, Some(fd), Some(fd))
                .map_err(|e| Error::vmm(format!("attaching the {name} console port"), e))?;
        }

        // Read-only at the device and not merely in the guest's mount flags. What is
        // behind this descriptor is the layer files, shared with every other image stacked
        // over them, so a guest allowed to write to it would be writing into those.
        let mut lower = BlockDevice::new("vda", named(&disk.path)?, DiskFormat::Vmdk)
            .map_err(|e| Error::vmm(format!("attaching {}", disk.path.display()), e))?;
        lower.set_read_only(true);

        let upper = BlockDevice::new("vdb", named(&rootfs.path)?, DiskFormat::Raw)
            .map_err(|e| Error::vmm(format!("attaching {}", rootfs.path.display()), e))?;

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
        if self.network.attached() {
            devices.add(net::attach(self.network, self.host_ports.clone())?);
        }

        // Nothing at all without the feature, which is what `gpu` promises: a build that
        // cannot serve Vulkan runs the session without it rather than refusing it.
        #[cfg(feature = "gpu")]
        if self.gpu {
            devices.add(super::gpu::device(self.gpu_memory_mib)?);
        }
        // In this order, and the order is the whole of what names them -- see the module
        // doc.
        devices.add(lower);
        devices.add(upper);

        // The shares last, since nothing about them depends on where they land. Read-only
        // at the device, which is the whole of what `Mount::read_only` promises -- see
        // [`Mount`].
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
        // is the first moment a rootfs nobody named can lose its name and still be
        // the disk the guest writes to. A named one is untouched. See
        // [`Ext4::discard`].
        rootfs.discard();

        vmm.run();

        // Reaching this line is a failure, and the one kind that can happen after a
        // machine has started. `run` is an event loop, and the VMM leaves it by `_exit`ing
        // this process with the guest's status -- so it comes back only when the loop
        // itself broke, which libkrun logs and then swallows. There is nothing left of the
        // machine to say more about it, which is why this error carries no source.
        Err(Error::EventLoop)
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
/// Refused here rather than in the guest: a relative mount point is mounted wherever the
/// init is standing, and the guest reports that -- if it reports it at all -- to a console
/// that is already showing a boot going wrong.
fn mount_point(mount: &Mount) -> Result<&str> {
    let at = named(&mount.at)?;
    match mount.at.is_absolute() {
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
/// image's `Cmd` and leaves its `Entrypoint` in front. An image that states neither runs
/// a shell, which is the only thing left that is certain to be there.
fn argv(config: &ImageState, asked: &[String]) -> Vec<String> {
    let mut argv = config.entrypoint.clone().unwrap_or_default();
    match asked.is_empty() {
        true => argv.extend(config.cmd.clone().unwrap_or_default()),
        false => argv.extend_from_slice(asked),
    }
    if argv.is_empty() {
        argv.push("/bin/sh".to_string());
    }
    argv
}

/// The environment the guest runs in: the image's, then the session's on top.
///
/// `PATH` and `HOME` are added when nothing set them, with the values docker uses. An
/// image that states neither is not unusual, and a session that cannot find `ls` is not
/// a useful one.
fn env(config: &ImageState, extra: &[String]) -> Vec<String> {
    let mut env = config.env.clone().unwrap_or_default();
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

    fn state(entrypoint: &[&str], cmd: &[&str]) -> ImageState {
        let some = |v: &[&str]| match v.is_empty() {
            true => None,
            false => Some(v.iter().map(|s| s.to_string()).collect()),
        };
        ImageState {
            entrypoint: some(entrypoint),
            cmd: some(cmd),
            ..Default::default()
        }
    }

    fn asked(argv: &[&str]) -> Vec<String> {
        argv.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn what_is_not_named_keeps_its_default() {
        let session = Session::new();
        assert_eq!(session.vcpus, 2);
        assert_eq!(session.ram_mib, 2048);
        assert_eq!(
            session.network,
            Network::Disabled,
            "a session that said nothing got a network"
        );
        assert!(session.host_ports.is_empty());
        assert!(!session.gpu);
        assert!(session.image.is_none());
        assert!(session.cache.is_none());
        assert!(session.rootfs.is_none());
        assert!(session.command.is_empty());
        assert!(session.env.is_empty());
    }

    /// A command is one argv, so the last word on it wins; an environment is a list, so
    /// every word on it is kept.
    #[test]
    fn a_command_replaces_and_an_environment_appends() {
        let session = Session::new()
            .command(["sh", "-c", "true"])
            .command(["uname", "-a"])
            .env(["LANG=C"])
            .env_var("TZ=UTC");

        assert_eq!(session.command, ["uname", "-a"]);
        assert_eq!(session.env, ["LANG=C", "TZ=UTC"]);
    }

    /// The one field with nothing to fall back on, caught before a throwaway layer is
    /// formatted for a machine that is not going to start.
    #[test]
    fn a_session_with_no_image_is_refused() {
        assert!(matches!(Session::new().enter(), Err(Error::NoImage)));
    }

    /// Kept as written, since a name is only parsed when the cache is asked about it --
    /// and a session is one image, so the last word on it wins.
    #[test]
    fn the_image_named_last_is_the_one_kept() {
        let session = Session::new().image("alpine:3.21").image("debian:trixie");
        assert_eq!(session.image.as_deref(), Some("debian:trixie"));
    }

    /// A rootfs handed over is the one the machine gets, rather than a path this
    /// makes something of its own from.
    #[test]
    fn the_rootfs_handed_over_is_the_one_kept() {
        let path = std::env::temp_dir().join(format!(
            "cortex-krun-handed-over-{}.ext4",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);

        let rootfs = Ext4::at(&path, 64 << 20).unwrap();
        let session = Session::new().rootfs(rootfs);

        let kept = session.rootfs.as_ref().unwrap();
        assert_eq!(kept.path, path);
        assert_eq!(kept.size, 64 << 20);
        assert!(!kept.scratch);
        let _ = std::fs::remove_file(&path);
    }

    /// One tag per mount, and the mounts' own order, since that is what pairs a device
    /// with the config entry that mounts it.
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

    /// Caught on the host, where there is a caller to tell -- see `mount_point`.
    #[test]
    fn a_mount_point_that_is_not_absolute_is_refused() {
        assert_eq!(mount_point(&Mount::new("/host", "/data")).unwrap(), "/data");
        assert!(matches!(
            mount_point(&Mount::new("/host", "data")),
            Err(Error::MountPoint { .. })
        ));
    }

    #[test]
    fn mounts_append_in_the_order_they_are_given() {
        let session = Session::new()
            .mount(Mount::new("/host/a", "/a"))
            .mounts([Mount::new("/host/b", "/b").read_only()]);

        let at: Vec<_> = session.mounts.iter().map(|m| m.at.as_path()).collect();
        assert_eq!(at, [Path::new("/a"), Path::new("/b")]);
        assert!(!session.mounts[0].read_only);
        assert!(session.mounts[1].read_only);
    }

    #[test]
    fn an_image_that_states_a_command_runs_it() {
        assert_eq!(argv(&state(&[], &["nginx", "-g"]), &[]), ["nginx", "-g"]);
        assert_eq!(
            argv(&state(&["/tini", "--"], &["nginx"]), &[]),
            ["/tini", "--", "nginx"]
        );
    }

    /// Docker's rule: what is asked for replaces the `Cmd` and keeps the `Entrypoint`.
    #[test]
    fn asking_for_a_command_replaces_the_cmd_and_not_the_entrypoint() {
        assert_eq!(
            argv(&state(&["/tini", "--"], &["nginx"]), &asked(&["sh"])),
            ["/tini", "--", "sh"]
        );
        assert_eq!(
            argv(&state(&[], &["nginx"]), &asked(&["sh", "-c", "hi"])),
            ["sh", "-c", "hi"]
        );
    }

    #[test]
    fn an_image_that_states_nothing_runs_a_shell() {
        assert_eq!(argv(&state(&[], &[]), &[]), ["/bin/sh"]);
    }

    #[test]
    fn a_path_the_image_did_not_set_is_added_and_one_it_did_is_left_alone() {
        let stated = ImageState {
            env: Some(vec!["PATH=/opt/bin".to_string()]),
            ..Default::default()
        };
        let stated = env(&stated, &[]);
        assert!(stated.contains(&"PATH=/opt/bin".to_string()), "{stated:?}");
        assert!(!stated.iter().any(|var| var == DEFAULT_PATH), "{stated:?}");

        let silent = env(&ImageState::default(), &[]);
        assert!(silent.iter().any(|var| var == DEFAULT_PATH), "{silent:?}");
        assert!(silent.contains(&"HOME=/root".to_string()), "{silent:?}");
    }

    /// Later wins in the environment a process is handed, so the session's copy of a
    /// name the image set is the one that takes effect.
    #[test]
    fn the_sessions_environment_goes_after_the_images() {
        let stated = ImageState {
            env: Some(vec!["LANG=C".to_string()]),
            ..Default::default()
        };
        let env = env(&stated, &asked(&["LANG=en_US.UTF-8"]));
        let at = |name: &str| env.iter().position(|var| var == name);
        assert!(at("LANG=C") < at("LANG=en_US.UTF-8"), "{env:?}");
    }
}
