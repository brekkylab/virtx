//! The virtio-gpu device a session gets when it asks for one.
//!
//! Only built for the `gpu` feature. A session that asks for a GPU without it is a
//! session that runs without one -- see [`Session::gpu`](super::Session::gpu).
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
//! Without it libkrun asks rutabaga for the virgl/virgl2 capsets, which makes
//! virglrenderer bring up an OpenGL context -- there is no EGL on macOS and it segfaults
//! in `create_gl_context`. venus needs none of it. libkrun does not expose the bit in its
//! own flag set, hence `from_bits_retain`.

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

/// A virtio-gpu device carrying venus, with a display backend that presents nothing.
pub(crate) fn device() -> Result<GpuDevice> {
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

    Ok(GpuDevice::new(
        VirglRendererFlags::VENUS
            | VirglRendererFlags::THREAD_SYNC
            | VirglRendererFlags::USE_ASYNC_FENCE_CB
            | NO_VIRGL,
        backend,
    ))
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
