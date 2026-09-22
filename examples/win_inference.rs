//! Boot a microVM on Windows and run a neural network on the guest GPU.
//!
//! Same venus-over-virtio-gpu stack as [`win_gpu.rs`](win_gpu.rs), with the two
//! changes an inference workload needs: a second virtiofs share holding the
//! ncnn source tree, the model and the build outputs, and enough RAM and vcpus
//! to compile ncnn inside the VM.
//!
//! `scripts/run-ncnn-squeezenet.ps1` populates the share and drives the build;
//! this example on its own just boots and runs a command against it.
//!
//!   .\scripts\run-ncnn-squeezenet.ps1                    # fetch, build, classify
//!   cargo run --features gpu --example win_inference     # re-run the classifier
//!   cargo run --features gpu --example win_inference -- 'ls /mnt/inference'

use std::io::{stderr, stdin, stdout};
use std::os::windows::io::AsHandle;
use std::path::PathBuf;

use krun::{
    BalloonDevice, ConsoleDevice, FsDevice, FsOverlay, MmioDeviceManager, Payload, RngDevice,
    TsiFlags, VmmBuilder, VsockDevice,
};

/// virtio-gpu always needs a display backend, even when nothing is ever
/// scanned out. venus only carries Vulkan traffic, so every framebuffer
/// method here reports "unsupported" and no frames are ever allocated.
#[cfg(feature = "gpu")]
mod headless_display {
    use krun_display::{
        DisplayBackendBasicFramebuffer, DisplayBackendError, DisplayBackendNew, Rect,
        ResourceFormat,
    };

    pub struct HeadlessDisplay;

    impl DisplayBackendNew<()> for HeadlessDisplay {
        fn new(_userdata: Option<&()>) -> Self {
            HeadlessDisplay
        }
    }

    impl DisplayBackendBasicFramebuffer for HeadlessDisplay {
        fn configure_scanout(
            &mut self,
            _scanout_id: u32,
            _display_width: u32,
            _display_height: u32,
            _width: u32,
            _height: u32,
            _format: ResourceFormat,
        ) -> Result<(), DisplayBackendError> {
            Err(DisplayBackendError::MethodNotSupported)
        }

        fn disable_scanout(&mut self, _scanout_id: u32) -> Result<(), DisplayBackendError> {
            Ok(())
        }

        fn alloc_frame(
            &mut self,
            _scanout_id: u32,
        ) -> Result<(u32, &mut [u8]), DisplayBackendError> {
            Err(DisplayBackendError::OutOfBuffers)
        }

        fn present_frame(
            &mut self,
            _scanout_id: u32,
            _frame_id: u32,
            _rect: Option<&Rect>,
        ) -> Result<(), DisplayBackendError> {
            Err(DisplayBackendError::MethodNotSupported)
        }
    }
}

/// Tag the guest kernel mounts as `/` (see `rootfstype=virtiofs` in libkrun's
/// default kernel cmdline).
const ROOTFS_TAG: &str = "/dev/root";

/// Second virtiofs share. Unlike the rootfs the guest does not mount this on
/// its own, so every command is prefixed with the `mount` below.
const WORKSPACE_TAG: &str = "inference";
const WORKSPACE_HOST_DIR: &str = "data/inference";
const WORKSPACE_GUEST_DIR: &str = "/mnt/inference";

/// Building ncnn is the expensive part; the inference itself needs very little.
const DEFAULT_RAM_MIB: u32 = 6144;
const MAX_VCPUS: usize = 8;

const DEFAULT_COMMAND: &str = concat!(
    "/mnt/inference/build/ncnn_squeezenet",
    " /mnt/inference/assets",
    " /mnt/inference/assets/dog.jpg"
);

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let command = std::env::args()
        .nth(1)
        .unwrap_or_else(|| DEFAULT_COMMAND.to_string());

    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));

    // The virtio-fs passthrough hands each root straight to `NtCreateFile`,
    // which -- unlike Win32 -- has no working directory and no notion of a
    // forward slash. `absolute` fixes both, and unlike `canonicalize` it does
    // not return a `\\?\` path, which `NtCreateFile` would reject in turn.
    let rootfs = std::path::absolute(manifest.join("data/rootfs/alpine"))?;
    if !rootfs.join("bin").join("busybox").exists() {
        return Err(format!(
            "no rootfs at {}; run .\\scripts\\fetch-alpine-rootfs.ps1 first",
            rootfs.display()
        )
        .into());
    }

    // Created rather than required: an empty share is a valid starting point,
    // and the provisioning script fills it from inside the VM.
    let workspace = std::path::absolute(manifest.join(WORKSPACE_HOST_DIR))?;
    std::fs::create_dir_all(&workspace)?;

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

    // Borrowed by the console device for as long as the VM runs.
    let (stdin, stdout, stderr) = (stdin(), stdout(), stderr());

    let mut payload = Payload::load_krunfw()?;

    let init = krun_init::Config::builder()
        .args(&[
            "/bin/sh",
            "-c",
            &format!(
                "set -e
                 mkdir -p {WORKSPACE_GUEST_DIR}
                 mount -t virtiofs {WORKSPACE_TAG} {WORKSPACE_GUEST_DIR}
                 {command}"
            ),
        ])
        .env(&[
            "PATH=/usr/sbin:/usr/bin:/sbin:/bin",
            "HOME=/root",
            "TERM=xterm",
        ])
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

    // The ncnn checkout, the build trees and the model live here instead of in
    // the rootfs: they are reproducible build artefacts, and `data/` is where
    // this repo keeps those.
    devices.add(FsDevice::new(WORKSPACE_TAG, &workspace.to_string_lossy())?);

    // Transparent Socket Impersonation, for the `apk add` the build needs.
    devices.add(VsockDevice::new(3, TsiFlags::HIJACK_INET)?);

    #[cfg(feature = "gpu")]
    {
        use headless_display::HeadlessDisplay;
        use krun::{DisplayInfoBuilder, GpuDevice, VirglRendererFlags};
        use krun_display::IntoDisplayBackend;

        let vtable = <HeadlessDisplay as IntoDisplayBackend<()>>::into_display_backend(None);
        // SAFETY: `vtable` is a fully populated krun_display_backend produced by
        // krun_display itself, and libkrun copies it out of the pointer.
        let mut backend = unsafe {
            krun::DisplayBackend::new(
                &raw const vtable as *const std::ffi::c_void,
                std::mem::size_of_val(&vtable),
            )
        }?;
        backend.add_display(DisplayInfoBuilder::new(1280, 800));

        // VIRGL_RENDERER_NO_VIRGL: venus needs no GL context, and the Windows
        // build has no GL backend to bring one up with. libkrun does not expose
        // the bit in its own flag set, hence from_bits_retain.
        const NO_VIRGL: VirglRendererFlags = VirglRendererFlags::from_bits_retain(1 << 7);

        devices.add(GpuDevice::new(
            VirglRendererFlags::VENUS
                | VirglRendererFlags::THREAD_SYNC
                | VirglRendererFlags::USE_ASYNC_FENCE_CB
                | NO_VIRGL,
            backend,
        ));
    }

    devices.add(RngDevice::new()?);
    devices.add(BalloonDevice::new()?);

    let vmm = VmmBuilder::new()
        .vcpus(vcpus())?
        .ram_mib(ram_mib())?
        .payload(payload)
        .devices(devices)
        .build()?;

    vmm.run();
    unreachable!("krun::Vmm::run does not return")
}

/// Compiling ncnn scales with cores, so take what the host has, capped so a
/// large host does not hand the guest more vcpus than the build can use.
fn vcpus() -> u8 {
    env_override("KRUN_VCPUS").unwrap_or_else(|| {
        std::thread::available_parallelism()
            .map(|n| n.get().min(MAX_VCPUS) as u8)
            .unwrap_or(4)
    })
}

fn ram_mib() -> u32 {
    env_override("KRUN_RAM_MIB").unwrap_or(DEFAULT_RAM_MIB)
}

fn env_override<T: std::str::FromStr>(name: &str) -> Option<T> {
    std::env::var(name).ok()?.parse().ok()
}
