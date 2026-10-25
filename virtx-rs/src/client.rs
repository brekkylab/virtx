//! The client: a `VirtxClient` held, and each request of `virtx.h` as a method.

use std::{
    ffi::{CString, c_void},
    ptr,
};

use bytes::Bytes;
use serde::{Serialize, de::DeserializeOwned};
use virtx_core::{
    DirEntry, ImageBuildArgs, ImageBuildOutput, ImageListOutputItem, ImagePullArgs,
    ImagePullOutput, ImageRemoveArgs, Metadata, MountSource, Recipe, SandboxCloseArgs,
    SandboxExecArgs, SandboxExecOutput, SandboxInitArgs, SandboxInitOutput, SandboxListOutputItem,
    SnapshotCreateArgs, SnapshotCreateOutput, SnapshotFileReadArgs, SnapshotFileReadDirArgs,
    SnapshotListOutputItem, SnapshotRemoveArgs, VolumeCreateArgs, VolumeCreateOutput,
    VolumeFileCopyArgs, VolumeFileCreateDirArgs, VolumeFileReadArgs, VolumeFileReadDirArgs,
    VolumeFileRemoveArgs, VolumeFileRenameArgs, VolumeFileStatArgs, VolumeFileWriteArgs,
    VolumeListOutputItem, VolumeMountCreateArgs, VolumeMountListArgs, VolumeMountListOutputItem,
    VolumeMountRemoveArgs, VolumeRemoveArgs,
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

    /// Reach the server at `url`, an `http` or `https` URL. Blocks until the client is made.
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

    /// Every reference the server holds, and the image each points at.
    pub fn list_image(&self) -> Call<Vec<ImageListOutputItem>> {
        Call::new(self.api, unsafe {
            (self.api.virtx_client_list_image)(self.raw)
        })
    }

    /// Pull `reference` from its registry, and point it at what it pulls. Its events are the
    /// pull's log, then what it pulled.
    pub fn pull_image(&self, reference: impl Into<String>) -> Events<ImagePullOutput> {
        let args = ImagePullArgs {
            reference: reference.into(),
            auth: None,
        };
        Events::new(self.api, self.start(self.api.virtx_client_pull_image, args))
    }

    /// Build `recipe`, and point `reference` at it. Its events are the build's log, then what
    /// it built.
    pub fn build_image(
        &self,
        recipe: Recipe,
        reference: impl Into<String>,
    ) -> Events<ImageBuildOutput> {
        let args = ImageBuildArgs {
            recipe,
            reference: reference.into(),
        };
        Events::new(
            self.api,
            self.start(self.api.virtx_client_build_image, args),
        )
    }

    /// Forget the image `digest` names, as `algorithm:hex`, and every reference pointing at it.
    pub fn remove_image(&self, digest: impl Into<String>) -> Call<()> {
        let args = ImageRemoveArgs {
            digest: digest.into(),
        };
        self.call(self.api.virtx_client_remove_image, args)
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

    /// The entries of the directory `path` in `snapshot`'s upper layer, in no set order; the
    /// image under it is not looked at.
    pub fn read_snapshot_dir(
        &self,
        snapshot: impl Into<String>,
        path: impl Into<String>,
    ) -> Call<Vec<DirEntry>> {
        let args = SnapshotFileReadDirArgs {
            snapshot: snapshot.into(),
            path: path.into(),
        };
        self.call(self.api.virtx_client_read_snapshot_dir, args)
    }

    /// Up to `length` bytes of the file `path` in `snapshot`'s upper layer, not the image under
    /// it, from `offset`, or to the end without a `length`. Fewer may come back before the end; none at or past it.
    pub fn read_snapshot_file(
        &self,
        snapshot: impl Into<String>,
        path: impl Into<String>,
        offset: u64,
        length: Option<u64>,
    ) -> Call<Bytes> {
        let args = SnapshotFileReadArgs {
            snapshot: snapshot.into(),
            path: path.into(),
            offset,
            length,
        };
        self.call(self.api.virtx_client_read_snapshot_file, args)
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

    /// The metadata of `path` in `volume`.
    pub fn stat_volume_file(
        &self,
        volume: impl Into<String>,
        path: impl Into<String>,
    ) -> Call<Metadata> {
        let args = VolumeFileStatArgs {
            volume: volume.into(),
            path: path.into(),
        };
        self.call(self.api.virtx_client_stat_volume_file, args)
    }

    /// The entries of the directory `path` in `volume`, in no set order.
    pub fn read_volume_dir(
        &self,
        volume: impl Into<String>,
        path: impl Into<String>,
    ) -> Call<Vec<DirEntry>> {
        let args = VolumeFileReadDirArgs {
            volume: volume.into(),
            path: path.into(),
        };
        self.call(self.api.virtx_client_read_volume_dir, args)
    }

    /// Up to `length` bytes of the file `path` in `volume` from `offset`, or to the end without
    /// a `length`. Fewer may come back before the end; none at or past it.
    pub fn read_volume_file(
        &self,
        volume: impl Into<String>,
        path: impl Into<String>,
        offset: u64,
        length: Option<u64>,
    ) -> Call<Bytes> {
        let args = VolumeFileReadArgs {
            volume: volume.into(),
            path: path.into(),
            offset,
            length,
        };
        self.call(self.api.virtx_client_read_volume_file, args)
    }

    /// Write `data` to the file `path` in `volume` at `offset`, or make the file `data` and
    /// nothing else without one. The file is created if there is none.
    pub fn write_volume_file(
        &self,
        volume: impl Into<String>,
        path: impl Into<String>,
        offset: Option<u64>,
        data: impl Into<Bytes>,
    ) -> Call<()> {
        let args = VolumeFileWriteArgs {
            volume: volume.into(),
            path: path.into(),
            offset,
            data: data.into(),
        };
        self.call(self.api.virtx_client_write_volume_file, args)
    }

    /// Create the directory `path` in `volume`, and with `parents` the ones above it too.
    pub fn create_volume_dir(
        &self,
        volume: impl Into<String>,
        path: impl Into<String>,
        parents: bool,
    ) -> Call<()> {
        let args = VolumeFileCreateDirArgs {
            volume: volume.into(),
            path: path.into(),
            parents,
        };
        self.call(self.api.virtx_client_create_volume_dir, args)
    }

    /// Delete the file or directory `path` in `volume`; a directory with anything in it only
    /// when `recursive`.
    pub fn remove_volume_file(
        &self,
        volume: impl Into<String>,
        path: impl Into<String>,
        recursive: bool,
    ) -> Call<()> {
        let args = VolumeFileRemoveArgs {
            volume: volume.into(),
            path: path.into(),
            recursive,
        };
        self.call(self.api.virtx_client_remove_volume_file, args)
    }

    /// Move `from` to `to` in `volume`, replacing what is at `to` only when `overwrite`.
    pub fn rename_volume_file(
        &self,
        volume: impl Into<String>,
        from: impl Into<String>,
        to: impl Into<String>,
        overwrite: bool,
    ) -> Call<()> {
        let args = VolumeFileRenameArgs {
            volume: volume.into(),
            from: from.into(),
            to: to.into(),
            overwrite,
        };
        self.call(self.api.virtx_client_rename_volume_file, args)
    }

    /// Copy `from`, a file or a whole directory, to `to` in `volume`, deleting what is at `to`
    /// first only when `overwrite`.
    pub fn copy_volume_file(
        &self,
        volume: impl Into<String>,
        from: impl Into<String>,
        to: impl Into<String>,
        overwrite: bool,
    ) -> Call<()> {
        let args = VolumeFileCopyArgs {
            volume: volume.into(),
            from: from.into(),
            to: to.into(),
            overwrite,
        };
        self.call(self.api.virtx_client_copy_volume_file, args)
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
    ) -> Events<SandboxExecOutput> {
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

    /// Start `request` with `args` as CBOR, which the library has read by the time it returns.
    fn start(
        &self,
        request: unsafe extern "C" fn(*mut Opaque, *const u8, usize) -> *mut Opaque,
        args: impl Serialize,
    ) -> *mut Opaque {
        let mut bytes = Vec::new();
        ciborium::into_writer(&args, &mut bytes).expect("a request's args encode");
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
