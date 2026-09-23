//! Boot a libkrun microVM with an Alpine rootfs and run a command in it.
//!
//!   ./scripts/fetch-alpine-rootfs.sh
//!   cargo run --example macos_hello
//!
//! On macOS the binary needs the `com.apple.security.hypervisor` entitlement;
//! `.cargo/macos-run.sh` ad-hoc signs it as part of `cargo run`.
//!
//! macOS only: the guest boots through Hypervisor.framework. Cargo cannot gate
//! an example by target -- `[target.'cfg(target_os = "macos")'.example]` is not
//! a manifest key -- so the whole example lives in `mod mac` and `main`
//! returns an error elsewhere, keeping `cargo build --examples` going on
//! other hosts.

#[cfg(target_os = "macos")]
mod mac {
    use std::io::{stderr, stdin, stdout};
    use std::os::fd::AsFd;
    use std::path::PathBuf;

    use krun::{
        BalloonDevice, ConsoleDevice, FsDevice, FsOverlay, MmioDeviceManager, Payload, RngDevice,
        VmmBuilder,
    };

    /// Tag the guest kernel mounts as `/` (see `rootfstype=virtiofs` in libkrun's
    /// default kernel cmdline).
    const ROOTFS_TAG: &str = "/dev/root";

    pub fn run() -> Result<(), Box<dyn std::error::Error>> {
        let rootfs = match std::env::args().nth(1) {
            Some(path) => PathBuf::from(path),
            None => PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("data/rootfs/alpine"),
        };
        if !rootfs.join("bin/busybox").exists() {
            return Err(format!(
                "no rootfs at {}; run ./scripts/fetch-alpine-rootfs.sh first",
                rootfs.display()
            )
            .into());
        }

        // Borrowed by the console device for as long as the VM runs, so these have
        // to outlive everything below.
        let (stdin, stdout, stderr) = (stdin(), stdout(), stderr());

        // Guest kernel, bundled as a shared library that libkrun dlopen()s.
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
            Some(stdin.as_fd()),
            Some(stdout.as_fd()),
            Some(stderr.as_fd()),
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
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(target_os = "macos")]
    {
        mac::run()
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err("macos_hello needs macOS; on Windows run `cargo run --example win_hello`".into())
    }
}
