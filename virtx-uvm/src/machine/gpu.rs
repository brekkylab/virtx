//! The virtio-gpu device a session gets when it asks for one.
//!
//! Only built for the `gpu` feature. A session that asks for a GPU without it is a
//! session that runs without one -- see [`Machine::gpu`](super::Machine::gpu).
//!
//! # What reaches the guest
//!
//! Vulkan, through [venus](https://docs.mesa3d.org/drivers/venus.html): the guest runs
//! Mesa's venus driver, virglrenderer serves it on the host, and the host's own driver --
//! MoltenVK on macOS, the vendor's on Windows -- does the work. The image has to carry
//! the guest half; on Alpine that is `mesa-vulkan-virtio vulkan-loader`, and a session
//! without it boots fine and finds no device.
//!
//! # The display nobody looks at
//!
//! virtio-gpu will not come up without a display backend and a scanout declared on it,
//! and venus carries Vulkan traffic and never scans anything out. So the backend here
//! reports every framebuffer call as unsupported and allocates no frames, and the scanout
//! exists to be counted rather than drawn to. A session is a command on a console, not a
//! window.
//!
//! # `NO_VIRGL`
//!
//! Without it libkrun advertises the virgl/virgl2 capsets, which the build has no vrend
//! behind (`-Dvrend=false`), and rutabaga leaves 2D resources and global fences to a
//! virglrenderer that does nothing with them; with it rutabaga handles both itself. venus
//! needs none of the GL side. libkrun does not expose the bit in its own flag set, hence
//! `from_bits_retain`.
//!
//! # `RENDER_SERVER`, everywhere but Windows
//!
//! Upstream virglrenderer serves venus only through its render server, and initialises
//! venus at all only when this bit is set. The macOS and Linux builds run that server as a
//! thread of this process (`render-server-mode=thread`, in `xtask/scripts/build-deps.sh`),
//! so the bit starts nothing. The Windows build has no render server and carries an
//! in-process venus context instead, which answers to `VENUS` alone -- with this bit set
//! there it would look for a server that was never built.
//!
//! Left off where there is a server, venus is never initialised and nothing says so: the
//! device still reports a venus capset of the right size, but zero-filled, and the guest's
//! driver refuses a wire format of version 0 without a word -- `vkEnumeratePhysicalDevices`
//! finds no GPU. That was every Linux boot while this bit was macOS's alone.
//!
//! # How much memory the GPU holds
//!
//! Every host-visible allocation the guest maps -- on a GPU that shares the host's memory,
//! nearly all of them -- is mapped through one window of guest-physical addresses, so the
//! window is what the guest's GPU can hold at once: a mapping past it fails `vkMapMemory`
//! with `VK_ERROR_OUT_OF_DEVICE_MEMORY`. It is libkrun's 8 GiB unless the session names a
//! size ([`Machine::gpu_memory_mib`](super::Machine::gpu_memory_mib)).
//!
//! The device the guest enumerates reports the same size: venus caps every heap, and
//! `VK_EXT_memory_budget`'s budgets, at `VKR_HEAP_SIZE_MAX` (see
//! virglrenderer's `heap-limit` commit), which is set here from the
//! window. Otherwise the guest sees the host device's whole heap -- 19 GB on an 18 GB Mac
//! -- sizes itself to that, and learns the truth from a mapping that fails; ncnn copies
//! out of the null pointer it gets back.
//!
//! What the window costs is the host's. On macOS and Linux libkrun reserves it without
//! committing it, and a blob's memory is mapped into it when the guest maps that blob; on
//! Windows vm-memory commits the whole window with the rest of guest memory, so it is charged
//! against the commit limit from boot whether or not the guest ever maps anything.

use krun::{DisplayInfoBuilder, GpuDevice, VirglRendererFlags};
use krun_display::{
    DisplayBackendBasicFramebuffer, DisplayBackendError, DisplayBackendNew, IntoDisplayBackend,
    Rect, ResourceFormat,
};

use super::{Error, Result};

/// `VIRGL_RENDERER_NO_VIRGL` -- see the module doc.
const NO_VIRGL: VirglRendererFlags = VirglRendererFlags::from_bits_retain(1 << 7);

/// The scanout that is declared and never drawn to. The numbers are arbitrary and the
/// device only wants there to be one.
const SCANOUT: (u32, u32) = (1280, 800);

/// The window, and the heaps, when a session names no size: libkrun's own default.
const DEFAULT_WINDOW: u64 = 8 << 30;

/// A virtio-gpu device carrying venus, with a display backend that presents nothing, whose
/// memory is `memory_mib` or [`DEFAULT_WINDOW`] -- see the module doc.
pub(crate) fn device(memory_mib: Option<u32>) -> Result<GpuDevice> {
    let window = memory_mib.map_or(DEFAULT_WINDOW, |mib| u64::from(mib) << 20);
    // SAFETY: nothing else runs yet -- libkrun starts its threads, and virglrenderer its
    // render server, only once the machine is built from this device.
    unsafe { std::env::set_var("VKR_HEAP_SIZE_MAX", window.to_string()) };

    let vtable = <Headless as IntoDisplayBackend<()>>::into_display_backend(None);

    // SAFETY: `vtable` is a fully populated krun_display_backend produced by krun_display
    // itself, and libkrun copies it out of the pointer rather than borrowing it -- so it
    // may be a local, and is.
    let mut backend = unsafe {
        krun::DisplayBackend::new(
            &raw const vtable as *const std::ffi::c_void,
            std::mem::size_of_val(&vtable),
        )
    }
    .map_err(|e| Error::vmm("making the guest's display backend", e))?;

    backend.add_display(DisplayInfoBuilder::new(SCANOUT.0, SCANOUT.1));

    let flags = VirglRendererFlags::VENUS
        | VirglRendererFlags::THREAD_SYNC
        | VirglRendererFlags::USE_ASYNC_FENCE_CB
        | NO_VIRGL;
    // See the module doc.
    #[cfg(not(windows))]
    let flags = flags | VirglRendererFlags::RENDER_SERVER;

    Ok(GpuDevice::new(flags, backend).shm_size(window as usize))
}

/// A display backend that exists so the device can come up, and does nothing else. See
/// the module doc.
struct Headless;

impl DisplayBackendNew<()> for Headless {
    fn new(_userdata: Option<&()>) -> Headless {
        Headless
    }
}

impl DisplayBackendBasicFramebuffer for Headless {
    fn configure_scanout(
        &mut self,
        _scanout_id: u32,
        _display_width: u32,
        _display_height: u32,
        _width: u32,
        _height: u32,
        _format: ResourceFormat,
    ) -> std::result::Result<(), DisplayBackendError> {
        Err(DisplayBackendError::MethodNotSupported)
    }

    fn disable_scanout(
        &mut self,
        _scanout_id: u32,
    ) -> std::result::Result<(), DisplayBackendError> {
        Ok(())
    }

    fn alloc_frame(
        &mut self,
        _scanout_id: u32,
    ) -> std::result::Result<(u32, &mut [u8]), DisplayBackendError> {
        Err(DisplayBackendError::OutOfBuffers)
    }

    fn present_frame(
        &mut self,
        _scanout_id: u32,
        _frame_id: u32,
        _rect: Option<&Rect>,
    ) -> std::result::Result<(), DisplayBackendError> {
        Err(DisplayBackendError::MethodNotSupported)
    }
}
