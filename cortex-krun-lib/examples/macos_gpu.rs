//! Boot a microVM with outbound networking and run a command in it.
//!
//! Used to check that guest Vulkan (venus over virtio-gpu) reaches the host's
//! Metal driver through MoltenVK.
//!
//!   # one-time: install the venus driver into the rootfs
//!   cargo run --example macos_gpu -- 'apk add --no-cache mesa-vulkan-virtio vulkan-loader vulkan-tools'
//!
//!   cargo run --features gpu --example macos_gpu
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

    pub fn run() -> Result<(), Box<dyn std::error::Error>> {
        let command = std::env::args()
            .nth(1)
            .unwrap_or_else(|| DEFAULT_COMMAND.to_string());

        // `data/` is at the repo root, a directory up from this package.
        let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .to_path_buf();
        let rootfs = repo.join("data/rootfs/alpine");
        if !rootfs.join("bin/busybox").exists() {
            return Err(format!(
                "no rootfs at {}; run ./scripts/fetch-alpine-rootfs.sh first",
                rootfs.display()
            )
            .into());
        }

        // Borrowed by the console device for as long as the VM runs.
        let (stdin, stdout, stderr) = (stdin(), stdout(), stderr());

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
            Some(stdin.as_fd()),
            Some(stdout.as_fd()),
            Some(stderr.as_fd()),
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
            // context -- there is no EGL on macOS and it segfaults in
            // create_gl_context. venus needs none of that. libkrun does not expose
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
            .vcpus(4)?
            .ram_mib(2048)?
            .payload(payload)
            .devices(devices)
            .build()?;

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
        Err("macos_gpu needs macOS: venus reaches Metal through MoltenVK here".into())
    }
}
