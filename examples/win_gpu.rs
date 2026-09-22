//! Boot a microVM on Windows with outbound networking, and run a command in it.
//!
//!   # one-time: install the venus driver into the rootfs, over TSI
//!   cargo run --example win_gpu -- 'apk add --no-cache mesa-vulkan-virtio vulkan-loader vulkan-tools'
//!
//!   cargo run --features gpu --example win_gpu
//!
//! This is [`win_hello.rs`](win_hello.rs) plus the two things guest Vulkan
//! needs: a TSI vsock, so `apk` can reach the network, and -- under the `gpu`
//! feature -- a virtio-gpu device wired to virglrenderer's venus context.
//!
//! The Windows counterpart of [`macos_gpu.rs`](macos_gpu.rs), and the same
//! division of labour: without `--features gpu` it is how the guest side gets
//! installed, with it the guest talks Vulkan to the host GPU.
//!
//! The `gpu` feature does not build on Windows yet -- see "Windows (WHP)" in
//! the README for what is missing. Without the feature this example is
//! complete, and is how the TSI path is exercised here.

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

const DEFAULT_COMMAND: &str = "vulkaninfo --summary";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let command = std::env::args()
        .nth(1)
        .unwrap_or_else(|| DEFAULT_COMMAND.to_string());

    // The virtio-fs passthrough hands its root straight to `NtCreateFile`,
    // which -- unlike Win32 -- has no working directory and no notion of a
    // forward slash. `absolute` fixes both, and unlike `canonicalize` it does
    // not return a `\\?\` path, which `NtCreateFile` would reject in turn.
    let rootfs = std::path::absolute(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("data/rootfs/alpine"),
    )?;
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

    // Borrowed by the console device for as long as the VM runs.
    let (stdin, stdout, stderr) = (stdin(), stdout(), stderr());

    let mut payload = Payload::load_krunfw()?;

    let init = krun_init::Config::builder()
        .args(&["/bin/sh", "-c", &command])
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

    // Transparent Socket Impersonation: the guest gets outbound connectivity
    // with no virtual interface and no helper process on the host, which is
    // all `apk` needs. `/etc/resolv.conf` in the rootfs points at a public
    // resolver; TSI proxies those UDP queries out too.
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
        // A scanout has to be declared for the device to come up, even though
        // this backend never presents to it.
        backend.add_display(DisplayInfoBuilder::new(1280, 800));

        // VIRGL_RENDERER_NO_VIRGL. Without it libkrun asks rutabaga for the
        // virgl/virgl2 capsets, which makes virglrenderer bring up an OpenGL
        // context. venus needs none of that, and the Windows build has no GL
        // backend to bring up. libkrun does not expose the bit in its own flag
        // set, hence from_bits_retain.
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
        .vcpus(4)?
        .ram_mib(2048)?
        .payload(payload)
        .devices(devices)
        .build()?;

    vmm.run();
    unreachable!("krun::Vmm::run does not return")
}
