//! Boot a libkrun microVM on Windows (WHP) and run a command in it.
//!
//!   .\scripts\fetch-alpine-rootfs.ps1
//!   cargo run --example win_hello
//!
//! This is [`hello.rs`](hello.rs) with the differences the Windows port forces.
//! The public API is `std::os::fd` everywhere else, so on Windows
//! `ConsoleBuilder` takes `BorrowedHandle` and the console handles come from
//! `AsHandle` rather than `AsFd`. libkrun also resolves the guest kernel as
//! `libkrunfw.dll` here; `scripts/windows-run.ps1`, registered as the cargo
//! runner, puts `data/krunfw/` on `PATH` so `LoadLibrary` finds it.
//!
//! Requires the *Windows Hypervisor Platform* optional feature. WHP is x86_64
//! only, so this will not run on Windows on ARM.

use std::io::{stderr, stdin, stdout};
use std::os::windows::io::AsHandle;
use std::path::PathBuf;

use krun::{
    BalloonDevice, ConsoleDevice, FsDevice, FsOverlay, MmioDeviceManager, Payload, RngDevice,
    VmmBuilder,
};

/// Tag the guest kernel mounts as `/` (see `rootfstype=virtiofs` in libkrun's
/// default kernel cmdline).
const ROOTFS_TAG: &str = "/dev/root";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let rootfs = match std::env::args().nth(1) {
        Some(path) => PathBuf::from(path),
        None => PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("data/rootfs/alpine"),
    };
    // The virtio-fs passthrough hands its root straight to `NtCreateFile`,
    // which -- unlike Win32 -- has no working directory and no notion of a
    // forward slash: a relative root aborts the VM, and `data/rootfs/alpine`
    // is ERROR_INVALID_NAME. `absolute` fixes both, and unlike `canonicalize`
    // it does not return a `\\?\` path, which `NtCreateFile` would reject in
    // turn.
    let rootfs = std::path::absolute(rootfs)?;
    if !rootfs.join("bin/busybox").exists() {
        return Err(format!(
            "no rootfs at {}; run .\\scripts\\fetch-alpine-rootfs.ps1 first",
            rootfs.display()
        )
        .into());
    }

    // KRUN_LOG=debug (or trace) turns on libkrun's own logging.
    if let Ok(level) = std::env::var("KRUN_LOG") {
        let level = match level.as_str() {
            "trace" => krun::LogLevel::Trace,
            "debug" => krun::LogLevel::Debug,
            "info" => krun::LogLevel::Info,
            _ => krun::LogLevel::Warn,
        };
        krun::init_log(None, level, krun::LogStyle::Auto, krun::LogOptions::empty())?;
    }

    // Borrowed by the console device for as long as the VM runs, so these have
    // to outlive everything below.
    let (stdin, stdout, stderr) = (stdin(), stdout(), stderr());

    // Guest kernel, bundled as a shared library that libkrun LoadLibrary()s.
    let mut payload = Payload::load_krunfw()?;

    // What PID 1 should do once the guest is up. The init binary itself and
    // this config are injected into the rootfs through a virtio-fs overlay,
    // and `apply` appends `init=/init.krun` to the kernel cmdline.
    let init = krun_init::Config::builder()
        .args(&["/bin/sh", "-c", "echo \"hello from $(uname -srm)\""])
        .env(&["PATH=/usr/sbin:/usr/bin:/sbin:/bin", "HOME=/root"])
        .workdir("/")
        .build();
    let mut overlay = FsOverlay::new();
    init.apply(&mut overlay, &mut payload)?;

    let mut devices = MmioDeviceManager::new();

    let mut console = ConsoleDevice::builder();
    console.add_default_console(
        Some(stdin.as_handle()),
        Some(stdout.as_handle()),
        Some(stderr.as_handle()),
    )?;
    devices.add(console.build()?);

    let mut root = FsDevice::new(ROOTFS_TAG, &rootfs.to_string_lossy())?;
    root.set_overlay(overlay);
    devices.add(root);

    devices.add(RngDevice::new()?);
    devices.add(BalloonDevice::new()?);

    let vmm = VmmBuilder::new()
        .vcpus(2)?
        .ram_mib(1024)?
        .payload(payload)
        .devices(devices)
        .build()?;

    // Runs the guest to completion and exits the process with its status.
    vmm.run();
    unreachable!("krun::Vmm::run does not return")
}
