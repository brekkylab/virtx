//! The client: a `VirtxClient` held, and each request of `virtx.h` as a method.

use std::{
    ffi::{CString, c_void},
    ptr,
};

use serde::{Serialize, de::DeserializeOwned};
use virtx_core::{
    ImageBuildArgs, ImageBuildOutput, MountSource, Recipe, SandboxCloseArgs, SandboxExecArgs,
    SandboxExecOutput, SandboxInitArgs, SandboxInitOutput, SandboxListOutputItem,
    SnapshotCreateArgs, SnapshotCreateOutput, SnapshotListOutputItem, SnapshotRemoveArgs,
    VolumeCreateArgs, VolumeCreateOutput, VolumeListOutputItem, VolumeMountCreateArgs,
    VolumeMountListArgs, VolumeMountListOutputItem, VolumeMountRemoveArgs, VolumeRemoveArgs,
};

use crate::{
    Error,
    call::{Call, Events, take},
    library::{Api, Opaque, VIRTX_OK, VirtxBytes, api},
};

/// A client of one server, over stdio or Streamable HTTP, made by `libvirtx_core`.
///
/// Cloning holds the same session once more; it ends when the last clone is dropped, which
/// answers every request not yet answered with an error. Each method starts its request at
/// once and returns a [`Call`] or [`Events`] to await, or to wait on without a runtime.
pub struct Client {
    api: &'static Api,
    raw: *mut Opaque,
}

// A `VirtxClient` is shared across threads by its reference count.
unsafe impl Send for Client {}
unsafe impl Sync for Client {}

impl Client {
    /// Start `program` with `args` as the server, and talk to it over its stdin and stdout.
    /// Blocks until it has started.
    pub fn spawn(
        program: impl AsRef<str>,
        args: impl IntoIterator<Item = impl AsRef<str>>,
    ) -> Result<Client, Error> {
        let api = api()?;
        let program = c_string(program.as_ref())?;
        let args = args
            .into_iter()
            .map(|arg| c_string(arg.as_ref()))
            .collect::<Result<Vec<_>, _>>()?;
        let argv: Vec<_> = args.iter().map(|arg| arg.as_ptr()).collect();
        let mut raw = ptr::null_mut();
        let mut error = VirtxBytes::empty();
        let status = unsafe {
            (api.virtx_client_spawn)(
                program.as_ptr(),
                argv.as_ptr(),
                argv.len(),
                &mut raw,
                &mut error,
            )
        };
        unsafe { opened(api, status, raw, error) }
    }

    /// Reach the server at `url`, its base `http` or `https` URL (`/rpc` is under it). Blocks
    /// until the client is made.
    pub fn connect(url: impl AsRef<str>) -> Result<Client, Error> {
        let api = api()?;
        let url = c_string(url.as_ref())?;
        let mut raw = ptr::null_mut();
        let mut error = VirtxBytes::empty();
        let status = unsafe { (api.virtx_client_connect)(url.as_ptr(), &mut raw, &mut error) };
        unsafe { opened(api, status, raw, error) }
    }

    /// The `VirtxClient*` this holds, still held by it: for another module over the same
    /// library to [`from_raw`](Client::from_raw), or to retain itself.
    pub fn as_raw(&self) -> *mut c_void {
        self.raw.cast()
    }

    /// Give up this hold of the `VirtxClient*`, for whoever takes the pointer to release.
    pub fn into_raw(self) -> *mut c_void {
        let raw = self.raw;
        std::mem::forget(self);
        raw.cast()
    }

    /// Take over one hold of `raw`, as [`into_raw`](Client::into_raw) or C's
    /// `virtx_client_retain` gave it.
    ///
    /// # Safety
    ///
    /// `raw` is a `VirtxClient*` made by the library this process opened (see
    /// [`library_path`](crate::library_path)), held once for the client returned.
    pub unsafe fn from_raw(raw: *mut c_void) -> Result<Client, Error> {
        Ok(Client {
            api: api()?,
            raw: raw.cast(),
        })
    }

    /// Build `recipe`, and point `reference` at it. Its events are the build's log, then what
    /// it built.
    pub fn build_image(
        &self,
        recipe: Recipe,
        reference: impl Into<String>,
    ) -> Events<ImageBuildOutput, String> {
        let args = ImageBuildArgs {
            recipe,
            reference: reference.into(),
        };
        Events::new(
            self.api,
            self.start(self.api.virtx_client_build_image, args),
        )
    }

    /// Every snapshot the server holds.
    pub fn list_snapshot(&self) -> Call<Vec<SnapshotListOutputItem>> {
        Call::new(self.api, unsafe {
            (self.api.virtx_client_list_snapshot)(self.raw)
        })
    }

    /// Keep a new, empty upper layer under `name`, holding at most `disk_gib` GiB; without
    /// either the server picks it.
    pub fn create_snapshot(
        &self,
        name: Option<impl Into<String>>,
        disk_gib: Option<u32>,
    ) -> Call<SnapshotCreateOutput> {
        let args = SnapshotCreateArgs {
            name: name.map(Into::into),
            disk_gib,
        };
        self.call(self.api.virtx_client_create_snapshot, args)
    }

    /// Delete the snapshot `name` and its upper layer.
    pub fn remove_snapshot(&self, name: impl Into<String>) -> Call<()> {
        let args = SnapshotRemoveArgs { name: name.into() };
        self.call(self.api.virtx_client_remove_snapshot, args)
    }

    /// Every volume the server holds.
    pub fn list_volume(&self) -> Call<Vec<VolumeListOutputItem>> {
        Call::new(self.api, unsafe {
            (self.api.virtx_client_list_volume)(self.raw)
        })
    }

    /// Keep a new, empty volume under `name`; without one the server picks it.
    pub fn create_volume(&self, name: Option<impl Into<String>>) -> Call<VolumeCreateOutput> {
        let args = VolumeCreateArgs {
            name: name.map(Into::into),
        };
        self.call(self.api.virtx_client_create_volume, args)
    }

    /// Delete the volume `name` and everything in it.
    pub fn remove_volume(&self, name: impl Into<String>) -> Call<()> {
        let args = VolumeRemoveArgs { name: name.into() };
        self.call(self.api.virtx_client_remove_volume, args)
    }

    /// The mounts in `volume`, without their credentials.
    pub fn list_volume_mount(
        &self,
        volume: impl Into<String>,
    ) -> Call<Vec<VolumeMountListOutputItem>> {
        let args = VolumeMountListArgs {
            volume: volume.into(),
        };
        self.call(self.api.virtx_client_list_volume_mount, args)
    }

    /// Mount `source` at `path` in `volume`, a directory the mount creates.
    pub fn create_volume_mount(
        &self,
        volume: impl Into<String>,
        path: impl Into<String>,
        source: MountSource,
    ) -> Call<()> {
        let args = VolumeMountCreateArgs {
            volume: volume.into(),
            path: path.into(),
            source,
        };
        self.call(self.api.virtx_client_create_volume_mount, args)
    }

    /// Take away the mount at `path` in `volume`, leaving its source as it is.
    pub fn remove_volume_mount(
        &self,
        volume: impl Into<String>,
        path: impl Into<String>,
    ) -> Call<()> {
        let args = VolumeMountRemoveArgs {
            volume: volume.into(),
            path: path.into(),
        };
        self.call(self.api.virtx_client_remove_volume_mount, args)
    }

    /// Every sandbox the server holds.
    pub fn list_sandbox(&self) -> Call<Vec<SandboxListOutputItem>> {
        Call::new(self.api, unsafe {
            (self.api.virtx_client_list_sandbox)(self.raw)
        })
    }

    /// Make a sandbox as `args` says; see [`SandboxInitArgs::new`].
    pub fn init_sandbox(&self, args: SandboxInitArgs) -> Call<SandboxInitOutput> {
        self.call(self.api.virtx_client_init_sandbox, args)
    }

    /// Run `cmd` in `sandbox` to its end, killed after `timeout_ms` if one is given. Its events
    /// are its output, a line as it is written, then how it ended.
    pub fn exec_sandbox(
        &self,
        sandbox: impl Into<String>,
        cmd: impl IntoIterator<Item = impl Into<String>>,
        timeout_ms: Option<u64>,
    ) -> Events<SandboxExecOutput, String> {
        let args = SandboxExecArgs {
            sandbox: sandbox.into(),
            cmd: cmd.into_iter().map(Into::into).collect(),
            timeout_ms,
        };
        Events::new(
            self.api,
            self.start(self.api.virtx_client_exec_sandbox, args),
        )
    }

    /// Stop `sandbox` and forget its id, keeping its snapshot and volumes.
    pub fn close_sandbox(&self, sandbox: impl Into<String>) -> Call<()> {
        let args = SandboxCloseArgs {
            sandbox: sandbox.into(),
        };
        self.call(self.api.virtx_client_close_sandbox, args)
    }

    /// Start `request` with `args` as JSON, which the library has read by the time it returns.
    fn start(
        &self,
        request: unsafe extern "C" fn(*mut Opaque, *const u8, usize) -> *mut Opaque,
        args: impl Serialize,
    ) -> *mut Opaque {
        let bytes = serde_json::to_vec(&args).expect("a request's args encode");
        unsafe { request(self.raw, bytes.as_ptr(), bytes.len()) }
    }

    fn call<T: DeserializeOwned>(
        &self,
        request: unsafe extern "C" fn(*mut Opaque, *const u8, usize) -> *mut Opaque,
        args: impl Serialize,
    ) -> Call<T> {
        Call::new(self.api, self.start(request, args))
    }
}

impl Clone for Client {
    fn clone(&self) -> Client {
        unsafe { (self.api.virtx_client_retain)(self.raw) };
        Client {
            api: self.api,
            raw: self.raw,
        }
    }
}

impl Drop for Client {
    /// The last one ends the session, blocking until it has.
    fn drop(&mut self) {
        unsafe { (self.api.virtx_client_release)(self.raw) };
    }
}

fn c_string(s: &str) -> Result<CString, Error> {
    CString::new(s).map_err(|_| Error::Argument(format!("{s:?} has a NUL in it")))
}

/// The client `status` says was opened, or the error written.
///
/// # Safety
///
/// `raw` and `error` are as `virtx_client_spawn` or `virtx_client_connect` wrote them with
/// `status`.
unsafe fn opened(
    api: &'static Api,
    status: i32,
    raw: *mut Opaque,
    error: VirtxBytes,
) -> Result<Client, Error> {
    if status == VIRTX_OK {
        return Ok(Client { api, raw });
    }
    match unsafe { take::<()>(api, status, error) } {
        Err(error) => Err(error),
        Ok(()) => Err(Error::Library(format!("unknown status {status}"))),
    }
}
