//! Booting an image with a writable layer over it, and handing this process to it.
//!
//! # What it takes
//!
//! Three things, and only one of them is made here:
//!
//! ```text
//!   the cache        ──► provision      ──► <stack>.vmdk   the image, read-only, shared
//!   the upper layer  ──► UpperLayer::at ──► session.ext4   this session's, written to
//!                                                │
//!                                                ▼
//!                                             a machine, which does not come back
//! ```
//!
//! The disk is [`provision`](crate::image::provision)'s and is made once per image,
//! whatever asks for it. The upper layer is made once per session, by the session. And
//! the stacking of the two is neither end's: it happens in the guest, where overlayfs is.
//!
//! # The devices, and what each is for
//!
//! ```text
//! virtio-fs  /dev/root   an empty root, holding the init and the directories it mounts
//!                        over -- replaced by the overlay before anything else runs
//! virtio-blk /dev/vda    the image's disk, read-only -- the overlay's lower
//! virtio-blk /dev/vdb    this session's ext4 -- the overlay's upper layer
//! virtio-con hvc0        the guest's console, on this process's own stdin and stdout
//! ```
//!
//! **Attach order is what fixes the two disk names**, and the guest is told which is
//! which by name: the lower goes on first and is `/dev/vda`, the upper layer second and
//! `/dev/vdb`. The pair of constants below and the order [`enter`] adds them in are one
//! statement made in two places, which is why they are next to each other.
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

use crate::image::{Cache, ImageState, Reference, provision};
use crate::session::{Error, Result, Session, UpperLayer};

/// The tag the guest kernel mounts as `/`, which libkrun's default kernel cmdline names
/// with `rootfstype=virtiofs`.
const ROOT_TAG: &str = "/dev/root";

/// Where the two disks land in the guest, in the order [`enter`] attaches them.
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

/// `S_IFDIR | 0755`, as [`FsOverlay::add_dir`] wants a mode.
const DIR_MODE: u32 = 0o040_755;

/// What a `PATH`-less image gets, which is what docker gives one.
const DEFAULT_PATH: &str = "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// Boot `reference` with `session`'s writable layer over it, run what it says, and become
/// the machine that does.
///
/// The image has to be in the cache already, exactly as
/// [`provision`](crate::image::provision) wants it -- this asks no registry anything.
/// `None` for the cache takes the same default [`pull`](crate::image::pull) writes to.
///
/// **This does not return.** It is not that the machine runs forever -- a session that
/// was asked for `echo hi` prints it and is over in seconds. It is that the way it ends
/// is *this process* ending: the VMM exits with the guest's own status, so
/// `enter(.., "exit 42", ..)` leaves the shell a 42 and everything a caller writes after
/// the call is unreachable. What comes back is only ever a failure to get that far,
/// which is what the [`Infallible`] says.
///
/// That the status arrives at all takes the patch below. The guest reports it as an
/// ioctl on the virtiofs mount, and a session has pivoted off that mount onto its
/// overlay by the time the workload exits -- so the init holds a descriptor on it across
/// the pivot. Without that every session exits 0, whatever it ran.
///
/// On macOS the calling binary needs the `com.apple.security.hypervisor` entitlement;
/// `scripts/macos-run.sh`, registered as the cargo runner, ad-hoc signs it for `cargo
/// run`. Hypervisor.framework refuses to start a VM without it.
///
/// # A session, and the next one
///
/// The upper layer is the session. One that is not there yet is formatted; one that is
/// there is carried on from, so a second call over the same path finds everything the
/// first wrote still in place -- including what it deleted, which overlayfs recorded as
/// a whiteout. Starting over is deleting that file.
///
/// ```no_run
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// use cortex_krun::session::{Session, enter};
///
/// enter("alpine:3.21", &Session {
///     command: vec!["/bin/sh".to_string(), "-c".to_string(), "uname -a".to_string()],
///     ..Session::at("/tmp/demo.ext4")
/// }, None)?;
/// # Ok(())
/// # }
/// ```
pub fn enter<E: Display>(
    reference: impl TryInto<Reference, Error = E>,
    session: &Session,
    cache: Option<Cache>,
) -> Result<Infallible> {
    // The image first. It is the one step that can fail on something the caller can still
    // do something about -- an image nobody pulled -- and doing it before the upper layer
    // is formatted means a mistyped reference does not leave a two-gigabyte file behind.
    let disk = provision(reference, cache)?;
    let upper_layer = UpperLayer::at(&session.upper_layer, session.size)?;

    // Borrowed by the console device for as long as the VM runs, so they outlive
    // everything below.
    let (stdout, stderr) = (stdout(), stderr());
    #[cfg(unix)]
    let stdin = stdin();

    // The guest kernel, which libkrun `dlopen`s rather than links.
    let mut payload =
        Payload::load_krunfw().map_err(|e| Error::vmm("loading the guest kernel", e))?;

    // What PID 1 does once the guest is up. The init binary and this config are injected
    // into the root through the overlay below, and `apply` is what appends
    // `init=/init.krun` to the kernel cmdline.
    //
    // The two devices go in as environment because that is how libkrun's init is told
    // where its root is -- see `patches/libkrun-85bed71-overlay-root.patch`. They reach
    // the workload's environment too, the way `KRUN_BLOCK_ROOT_*` already does.
    let argv = argv(&disk.config, &session.command);
    let env = env(&disk.config, &session.env);
    let init = krun_init::Config::builder()
        .args(&argv.iter().map(String::as_str).collect::<Vec<_>>())
        .env(&env.iter().map(String::as_str).collect::<Vec<_>>())
        .workdir(disk.config.working_dir.as_deref().unwrap_or("/"))
        .env_var(&format!("KRUN_BLOCK_LOWER_DEVICE={LOWER_DEVICE}"))
        .env_var(&format!("KRUN_BLOCK_LOWER_FSTYPE={LOWER_FSTYPE}"))
        .env_var(&format!("KRUN_BLOCK_UPPER_DEVICE={UPPER_DEVICE}"))
        .env_var(&format!("KRUN_BLOCK_UPPER_FSTYPE={UPPER_FSTYPE}"))
        .build();

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

    // Read-only at the device and not merely in the guest's mount flags. What is behind
    // this descriptor is the layer files, shared with every other image stacked over
    // them, so a guest allowed to write to it would be writing into those.
    let mut lower = BlockDevice::new("vda", named(&disk.path)?, DiskFormat::Vmdk)
        .map_err(|e| Error::vmm(format!("attaching {}", disk.path.display()), e))?;
    lower.set_read_only(true);

    let upper_layer = BlockDevice::new("vdb", named(&upper_layer.path)?, DiskFormat::Raw)
        .map_err(|e| Error::vmm(format!("attaching {}", upper_layer.path.display()), e))?;

    let mut devices = MmioDeviceManager::new();
    devices.add(root);
    devices.add(
        console
            .build()
            .map_err(|e| Error::vmm("building the guest console", e))?,
    );
    devices.add(RngDevice::new().map_err(|e| Error::vmm("attaching an entropy source", e))?);
    devices.add(BalloonDevice::new().map_err(|e| Error::vmm("attaching a balloon", e))?);
    // In this order, and the order is the whole of what names them -- see the module doc.
    devices.add(lower);
    devices.add(upper_layer);

    let vmm = VmmBuilder::new()
        .vcpus(session.vcpus)
        .map_err(|e| Error::vmm(format!("{} vCPUs", session.vcpus), e))?
        .ram_mib(session.ram_mib)
        .map_err(|e| Error::vmm(format!("{} MiB of memory", session.ram_mib), e))?
        .payload(payload)
        .devices(devices)
        .build()
        .map_err(|e| Error::vmm("building the machine", e))?;

    vmm.run();

    // Reaching this line is a failure, and the one kind that can happen after a machine
    // has started. `run` is an event loop, and the VMM leaves it by `_exit`ing this
    // process with the guest's status -- so it comes back only when the loop itself
    // broke, which libkrun logs and then swallows. There is nothing left of the machine
    // to say more about it, which is why this error carries no source.
    Err(Error::EventLoop)
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
