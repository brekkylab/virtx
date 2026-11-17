//! The client: a `VirtxClient` held, and each request of `virtx.h` as a method.

use std::{
    ffi::{CString, c_void},
    future::Future,
    mem::ManuallyDrop,
    path::PathBuf,
    pin::Pin,
    ptr,
    task::{Context, Poll},
};

use serde::{Serialize, de::DeserializeOwned};
use serde_json::json;
use virtx_core::{
    Auth, DirEntry, Event, FileMoveArgs, FilePathArgs, FileReadArgs, FileRemoveArgs, FileRoot,
    FileWriteArgs, ImageBuildArgs, ImageDownloadArgs, ImageDownloadOutput,
    ImageJob as ImageJobInfo, ImageJobEntry, ImageListOutputItem, ImagePruneOutput, ImagePullArgs,
    ImagePushArgs, ImageRemoveArgs, ImageTagArgs, ImageUntagArgs, ImageUploadArgs,
    ImageUploadOutput, LogEntry, Metadata, MountSource, Recipe, RegistryAuth,
    Sandbox as SandboxInfo, SandboxCreateArgs, SandboxExecArgs, SandboxJob as SandboxJobInfo,
    SandboxJobEntry, SnapshotCreateArgs, SnapshotListOutputItem as SnapshotInfo, VolumeCreateArgs,
    VolumeListOutputItem as VolumeInfo, VolumeMountListOutputItem,
};

use crate::{
    Error, ImageJob, Sandbox, SandboxJob, Snapshot, Volume,
    call::{Call, Events, take},
    job::StartedJob,
    library::{Api, Opaque, VIRTX_OK, VirtxBytes, api},
};

/// A client of one server, over stdio or HTTP, made by `libvirtx_core`.
///
/// Cloning holds the same session once more; it ends when the last clone is dropped, which
/// answers every request not yet answered with an error. Each method starts its request at
/// once and returns a [`Call`] or [`Events`] to await, or to wait on without a runtime.
pub struct Client {
    pub(crate) api: &'static Api,
    pub(crate) raw: *mut Opaque,
}

// A `VirtxClient` is shared across threads by its reference count.
unsafe impl Send for Client {}
unsafe impl Sync for Client {}

/// Fetch `virtx-stdio`, the server of the library's version, into this host's cache if it is
/// not there, and answer its path: what [`Client::spawn_virtx`] starts.
///
/// The server is kept one directory per version, so a library that is upgraded fetches its
/// own. `$VIRTX_STDIO` names a server to use as it is; `$VIRTX_STDIO_VERSION` another version;
/// `$VIRTX_DIST_URL` where releases are fetched from; `$VIRTX_HOME` the cache.
pub fn ensure_virtx() -> Result<Call<PathBuf>, Error> {
    let api = api()?;
    Ok(Call::<String>::new(api, unsafe { (api.virtx_ensure_virtx)() }).map(PathBuf::from))
}

/// `virtx-stdio` being started by [`Client::spawn_virtx`]: awaited, or waited for without a
/// runtime with [`wait`](Opening::wait), it answers the client. The fetch runs from when this
/// is made; dropping it cancels it.
#[must_use = "an opening dropped is cancelled"]
pub struct Opening {
    /// The fetch, or why none could start, answered once.
    fetch: Result<Call<PathBuf>, Option<Error>>,
    args: Vec<String>,
}

impl Opening {
    /// Block this thread until the server has started, for a caller without an async runtime.
    pub fn wait(self) -> Result<Client, Error> {
        let path = match self.fetch {
            Ok(fetch) => fetch.wait()?,
            Err(error) => return Err(error.expect("waited for once")),
        };
        Client::spawn(path.to_string_lossy(), &self.args)
    }
}

impl Future for Opening {
    type Output = Result<Client, Error>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let path = match &mut self.fetch {
            Ok(fetch) => match Pin::new(fetch).poll(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                Poll::Ready(Ok(path)) => path,
            },
            Err(error) => return Poll::Ready(Err(error.take().expect("polled after ready"))),
        };
        Poll::Ready(Client::spawn(path.to_string_lossy(), &self.args))
    }
}

impl Client {
    /// Start `program` with `args` as the server, and talk to it over its stdin and stdout.
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

    /// Start `virtx-stdio`, the server of the library's version, with `args`, fetching it first
    /// if this host has none -- as [`ensure_virtx`] does, which does the fetch alone.
    pub fn spawn_virtx(args: impl IntoIterator<Item = impl AsRef<str>>) -> Opening {
        Opening {
            fetch: ensure_virtx().map_err(Some),
            args: args
                .into_iter()
                .map(|arg| arg.as_ref().to_owned())
                .collect(),
        }
    }

    /// Let go of this hold of the session, and answer once it is let go of: for the last
    /// hold, once the session has ended. Dropping a client does the same, blocking for it.
    pub fn close(self) -> Call<()> {
        let this = ManuallyDrop::new(self);
        Call::new(this.api, unsafe {
            (this.api.virtx_client_release)(this.raw)
        })
    }

    /// Reach the server at `url`, its base `http` or `https` URL sending
    /// `auth`, the credentials it wants, and `headers`, each a name and a value, whatever else
    /// it asks of each request, with every request. `Authorization` is `auth`'s, over one given
    /// as a header. Refused when a token would go in the clear: over `http` to any host but
    /// this machine. Blocks until the client is made.
    pub fn connect(
        url: impl AsRef<str>,
        auth: Auth,
        headers: impl IntoIterator<Item = (impl AsRef<str>, impl AsRef<str>)>,
    ) -> Result<Client, Error> {
        let api = api()?;
        let url = c_string(url.as_ref())?;
        let auth = serde_json::to_vec(&auth).expect("an auth encodes");
        let strings = header_strings(headers)?;
        let pointers: Vec<_> = strings.iter().map(|s| s.as_ptr()).collect();
        let mut raw = ptr::null_mut();
        let mut error = VirtxBytes::empty();
        let status = unsafe {
            (api.virtx_client_connect)(
                url.as_ptr(),
                auth.as_ptr(),
                auth.len(),
                pointers.as_ptr(),
                strings.len() / 2,
                &mut raw,
                &mut error,
            )
        };
        unsafe { opened(api, status, raw, error) }
    }

    /// Send `auth` with every request from now on, in place of what was given at
    /// [`connect`](Self::connect): a new token, when the server's are short-lived. A forwarder
    /// already open sends it with its next connection. Over stdio, where none is sent, nothing
    /// changes. Refused as at `connect`, the auth unchanged.
    pub fn set_auth(&self, auth: Auth) -> Result<(), Error> {
        let auth = serde_json::to_vec(&auth).expect("an auth encodes");
        let mut error = VirtxBytes::empty();
        let status = unsafe {
            (self.api.virtx_client_set_auth)(self.raw, auth.as_ptr(), auth.len(), &mut error)
        };
        if status == VIRTX_OK {
            return Ok(());
        }
        unsafe { take(self.api, status, error) }
    }

    /// Send `headers` with every request from now on, in place of those given at
    /// [`connect`](Self::connect). A forwarder already open sends them with its next
    /// connection. Over stdio, where none are sent, nothing changes. A header that cannot be
    /// sent is refused, the headers unchanged.
    pub fn set_headers(
        &self,
        headers: impl IntoIterator<Item = (impl AsRef<str>, impl AsRef<str>)>,
    ) -> Result<(), Error> {
        let strings = header_strings(headers)?;
        let pointers: Vec<_> = strings.iter().map(|s| s.as_ptr()).collect();
        let mut error = VirtxBytes::empty();
        let status = unsafe {
            (self.api.virtx_client_set_headers)(
                self.raw,
                pointers.as_ptr(),
                strings.len() / 2,
                &mut error,
            )
        };
        if status == VIRTX_OK {
            return Ok(());
        }
        unsafe { take(self.api, status, error) }
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

    /// Start building `recipe`, to point `reference` at what it builds. Its log is
    /// [`ImageJob::log`]'s, or [`StartedJob::log`].
    pub fn build_image(
        &self,
        recipe: Recipe,
        reference: impl Into<String>,
    ) -> StartedJob<ImageJob, ImageJobEntry> {
        let args = ImageBuildArgs {
            recipe,
            reference: reference.into(),
        };
        self.start_image_job(self.call(self.api.virtx_client_build_image, args))
    }

    /// Start pulling `reference` from its registry, to point it at what it pulls; without
    /// `auth` the server pulls anonymously. Its log is [`ImageJob::log`]'s,
    /// or [`StartedJob::log`].
    pub fn pull_image(
        &self,
        reference: impl Into<String>,
        auth: Option<RegistryAuth>,
    ) -> StartedJob<ImageJob, ImageJobEntry> {
        let args = ImagePullArgs {
            reference: reference.into(),
            auth,
        };
        self.start_image_job(self.call(self.api.virtx_client_pull_image, args))
    }

    /// Start pushing the image `reference` points at to the registry `reference` names;
    /// without `auth` the server pushes anonymously. Its log is
    /// [`ImageJob::log`]'s, or [`StartedJob::log`].
    pub fn push_image(
        &self,
        reference: impl Into<String>,
        auth: Option<RegistryAuth>,
    ) -> StartedJob<ImageJob, ImageJobEntry> {
        let args = ImagePushArgs {
            reference: reference.into(),
            auth,
        };
        self.start_image_job(self.call(self.api.virtx_client_push_image, args))
    }

    /// Every image job the server holds.
    pub fn list_image_job(&self) -> Call<Vec<ImageJob>> {
        let client = self.clone();
        let call: Call<Vec<ImageJobInfo>> = Call::new(self.api, unsafe {
            (self.api.virtx_client_list_image_job)(self.raw)
        });
        call.map(move |infos| {
            infos
                .into_iter()
                .map(|info| ImageJob::new(client.clone(), info))
                .collect()
        })
    }

    /// The image job `info` describes, reached through this client, without asking the server.
    pub fn image_job(&self, info: ImageJobInfo) -> ImageJob {
        ImageJob::new(self.clone(), info)
    }

    /// The image job `id`.
    pub fn get_image_job(&self, id: impl AsRef<str>) -> Call<ImageJob> {
        let args = json!({ "id": id.as_ref() });
        let client = self.clone();
        let call: Call<ImageJobInfo> = self.call(self.api.virtx_client_get_image_job, args);
        call.map(move |info| ImageJob::new(client.clone(), info))
    }

    /// Cancel the image job `id` if it runs, and forget it.
    pub(crate) fn remove_image_job(&self, id: impl AsRef<str>) -> Call<()> {
        let args = json!({ "id": id.as_ref() });
        self.call(self.api.virtx_client_remove_image_job, args)
    }

    /// The log of the image job `id`, from its start or after its `after`th entry, until its
    /// `end` entry. Dropping it leaves the job as it is.
    pub(crate) fn image_job_log(
        &self,
        id: impl AsRef<str>,
        after: Option<u64>,
    ) -> Events<LogEntry<ImageJobEntry>> {
        let args = json!({ "id": id.as_ref(), "after": after });
        Events::new(
            self.api,
            self.start(self.api.virtx_client_image_job_log, args),
        )
    }

    /// Every tag on the server's images, as `name:tag`, with the digest it points at.
    pub fn list_image(&self) -> Call<Vec<ImageListOutputItem>> {
        Call::new(self.api, unsafe {
            (self.api.virtx_client_list_image)(self.raw)
        })
    }

    /// Point the tag `target` (`name:tag`) at the image `source` names (`name:tag` or
    /// `name@algorithm:hex`), moving it if it is in use.
    pub fn tag_image(&self, source: impl Into<String>, target: impl Into<String>) -> Call<()> {
        let args = ImageTagArgs {
            source: source.into(),
            target: target.into(),
        };
        self.call(self.api.virtx_client_tag_image, args)
    }

    /// Remove the tag `reference` (`name:tag`), leaving the image it points at.
    pub fn untag_image(&self, reference: impl Into<String>) -> Call<()> {
        let args = ImageUntagArgs {
            reference: reference.into(),
        };
        self.call(self.api.virtx_client_untag_image, args)
    }

    /// Remove the image `reference` names (`name:tag` or `name@algorithm:hex`), and every tag on
    /// it in that name.
    pub fn remove_image(&self, reference: impl Into<String>) -> Call<()> {
        let args = ImageRemoveArgs {
            reference: reference.into(),
        };
        self.call(self.api.virtx_client_remove_image, args)
    }

    /// Write the image `reference` names to `path`, as an OCI image layout in a tar archive,
    /// replacing what is there once all of it is written. Its events are a line of log a blob,
    /// then what it wrote.
    pub fn download_image(
        &self,
        reference: impl Into<String>,
        path: impl Into<PathBuf>,
    ) -> Events<Event<ImageDownloadOutput, String>> {
        let args = ImageDownloadArgs {
            reference: reference.into(),
            path: path.into(),
        };
        Events::new(
            self.api,
            self.start(self.api.virtx_client_download_image, args),
        )
    }

    /// Upload the image in the OCI image layout archive at `path`, and point the tag
    /// `reference` (`name:tag`) at it: the archive's only image, or the one it names by that
    /// tag. Its events are a line of log a blob, then what it uploaded.
    pub fn upload_image(
        &self,
        path: impl Into<PathBuf>,
        reference: impl Into<String>,
    ) -> Events<Event<ImageUploadOutput, String>> {
        let args = ImageUploadArgs {
            path: path.into(),
            reference: reference.into(),
        };
        Events::new(
            self.api,
            self.start(self.api.virtx_client_upload_image, args),
        )
    }

    /// Delete each image no tag points at and no sandbox uses, and what only such images held.
    pub fn prune_image(&self) -> Call<ImagePruneOutput> {
        Call::new(self.api, unsafe {
            (self.api.virtx_client_prune_image)(self.raw)
        })
    }

    /// Every snapshot the server holds.
    pub fn list_snapshot(&self) -> Call<Vec<Snapshot>> {
        let client = self.clone();
        let call: Call<Vec<SnapshotInfo>> = Call::new(self.api, unsafe {
            (self.api.virtx_client_list_snapshot)(self.raw)
        });
        call.map(move |infos| {
            infos
                .into_iter()
                .map(|info| Snapshot::new(client.clone(), info))
                .collect()
        })
    }

    /// Keep a new, empty upper layer under `name`, holding at most `disk_gib` GiB; without
    /// either the server picks it.
    pub fn create_snapshot(
        &self,
        name: Option<impl Into<String>>,
        disk_gib: Option<u32>,
    ) -> Call<Snapshot> {
        let args = SnapshotCreateArgs {
            name: name.map(Into::into),
            disk_gib,
        };
        let client = self.clone();
        let call: Call<SnapshotInfo> = self.call(self.api.virtx_client_create_snapshot, args);
        call.map(move |info| Snapshot::new(client.clone(), info))
    }

    /// The snapshot `info` describes, reached through this client, without asking the server.
    pub fn snapshot(&self, info: SnapshotInfo) -> Snapshot {
        Snapshot::new(self.clone(), info)
    }

    /// The snapshot `name`.
    pub fn get_snapshot(&self, name: impl AsRef<str>) -> Call<Snapshot> {
        let args = json!({ "name": name.as_ref() });
        let client = self.clone();
        let call: Call<SnapshotInfo> = self.call(self.api.virtx_client_get_snapshot, args);
        call.map(move |info| Snapshot::new(client.clone(), info))
    }

    /// Delete the snapshot `name` and its upper layer.
    pub(crate) fn remove_snapshot(&self, name: impl AsRef<str>) -> Call<()> {
        let args = json!({ "name": name.as_ref() });
        self.call(self.api.virtx_client_remove_snapshot, args)
    }

    /// Every volume the server holds.
    pub fn list_volume(&self) -> Call<Vec<Volume>> {
        let client = self.clone();
        let call: Call<Vec<VolumeInfo>> = Call::new(self.api, unsafe {
            (self.api.virtx_client_list_volume)(self.raw)
        });
        call.map(move |infos| {
            infos
                .into_iter()
                .map(|info| Volume::new(client.clone(), info))
                .collect()
        })
    }

    /// Keep a new, empty volume under `name`; without one the server picks it.
    pub fn create_volume(&self, name: Option<impl Into<String>>) -> Call<Volume> {
        let args = VolumeCreateArgs {
            name: name.map(Into::into),
        };
        let client = self.clone();
        let call: Call<VolumeInfo> = self.call(self.api.virtx_client_create_volume, args);
        call.map(move |info| Volume::new(client.clone(), info))
    }

    /// The volume `info` describes, reached through this client, without asking the server.
    pub fn volume(&self, info: VolumeInfo) -> Volume {
        Volume::new(self.clone(), info)
    }

    /// The volume `name`.
    pub fn get_volume(&self, name: impl AsRef<str>) -> Call<Volume> {
        let args = json!({ "name": name.as_ref() });
        let client = self.clone();
        let call: Call<VolumeInfo> = self.call(self.api.virtx_client_get_volume, args);
        call.map(move |info| Volume::new(client.clone(), info))
    }

    /// Delete the volume `name` and everything in it.
    pub(crate) fn remove_volume(&self, name: impl AsRef<str>) -> Call<()> {
        let args = json!({ "name": name.as_ref() });
        self.call(self.api.virtx_client_remove_volume, args)
    }

    /// The mounts in `volume`, without their credentials.
    pub(crate) fn list_volume_mount(
        &self,
        volume: impl AsRef<str>,
    ) -> Call<Vec<VolumeMountListOutputItem>> {
        let args = json!({ "volume": volume.as_ref() });
        self.call(self.api.virtx_client_list_volume_mount, args)
    }

    /// Mount `source` at `path` in `volume`, a directory the mount creates.
    pub(crate) fn create_volume_mount(
        &self,
        volume: impl AsRef<str>,
        path: impl AsRef<str>,
        source: MountSource,
    ) -> Call<VolumeMountListOutputItem> {
        let args = json!({ "volume": volume.as_ref(), "path": path.as_ref(), "source": source });
        self.call(self.api.virtx_client_create_volume_mount, args)
    }

    /// Take away the mount at `path` in `volume`, leaving its source as it is.
    pub(crate) fn remove_volume_mount(
        &self,
        volume: impl AsRef<str>,
        path: impl AsRef<str>,
    ) -> Call<()> {
        let args = json!({ "volume": volume.as_ref(), "path": path.as_ref() });
        self.call(self.api.virtx_client_remove_volume_mount, args)
    }

    /// What is at `path` in `root`, a path relative to its root as `dir/file.txt`.
    pub(crate) fn stat_file(&self, root: &FileRoot, path: impl Into<String>) -> Call<Metadata> {
        self.call(self.api.virtx_client_stat_file, self.at(root, path))
    }

    /// Whether anything is at `path` in `root`.
    pub(crate) fn exists(&self, root: &FileRoot, path: impl Into<String>) -> Call<bool> {
        self.call(self.api.virtx_client_file_exists, self.at(root, path))
    }

    /// Whether a file is at `path` in `root`.
    pub(crate) fn is_file(&self, root: &FileRoot, path: impl Into<String>) -> Call<bool> {
        self.call(self.api.virtx_client_is_file, self.at(root, path))
    }

    /// Whether a directory is at `path` in `root`.
    pub(crate) fn is_dir(&self, root: &FileRoot, path: impl Into<String>) -> Call<bool> {
        self.call(self.api.virtx_client_is_dir, self.at(root, path))
    }

    /// The entries of the directory at `path` in `root`.
    pub(crate) fn read_dir(&self, root: &FileRoot, path: impl Into<String>) -> Call<Vec<DirEntry>> {
        self.call(self.api.virtx_client_read_dir, self.at(root, path))
    }

    /// The file at `path` in `root`, from `offset`, `length` bytes of it or all that is left.
    pub(crate) fn read_file(
        &self,
        root: &FileRoot,
        path: impl Into<String>,
        offset: u64,
        length: Option<u64>,
    ) -> Call<Vec<u8>> {
        let args = FileReadArgs {
            root: root.clone(),
            path: path.into(),
            offset,
            length,
        };
        Call::raw(self.api, self.start(self.api.virtx_client_read_file, args))
    }

    /// The whole file at `path` in `root` as text, which it has to be: UTF-8.
    pub(crate) fn read_text(&self, root: &FileRoot, path: impl Into<String>) -> Call<String> {
        self.call(self.api.virtx_client_read_text, self.at(root, path))
    }

    /// Write `content` as the whole file at `path` in `root`, creating it if it does not
    /// exist. Its parent directory must exist. With `if_match`, the `etag` a [`Metadata`] of
    /// the file gave, the write goes only while the file still has it, and is refused with a
    /// `412` otherwise.
    pub(crate) fn write_file(
        &self,
        root: &FileRoot,
        path: impl Into<String>,
        content: impl AsRef<[u8]>,
        if_match: Option<&str>,
    ) -> Call<()> {
        let args = FileWriteArgs {
            root: root.clone(),
            path: path.into(),
            if_match: if_match.map(str::to_owned),
        };
        let args = serde_json::to_vec(&args).expect("a request's args encode");
        let content = content.as_ref();
        let future = unsafe {
            (self.api.virtx_client_write_file)(
                self.raw,
                args.as_ptr(),
                args.len(),
                content.as_ptr(),
                content.len(),
            )
        };
        Call::new(self.api, future)
    }

    /// Create the directory at `path` in `root`, whose parent must exist.
    pub(crate) fn create_dir(&self, root: &FileRoot, path: impl Into<String>) -> Call<()> {
        self.call(self.api.virtx_client_create_dir, self.at(root, path))
    }

    /// Have the directory at `path` in `root`, as `mkdir -p`: its parents are created as
    /// needed, and one there already is left as it is. Refused with a `405` when a file is at
    /// `path`.
    pub(crate) fn create_dir_all(&self, root: &FileRoot, path: impl Into<String>) -> Call<()> {
        self.call(self.api.virtx_client_create_dir_all, self.at(root, path))
    }

    /// Remove what is at `path` in `root`: a directory with everything in it. With
    /// `missing_ok`, nothing being there is fine, as `rm -f`; without it, a `404`.
    pub(crate) fn remove_file(
        &self,
        root: &FileRoot,
        path: impl Into<String>,
        missing_ok: bool,
    ) -> Call<()> {
        let args = FileRemoveArgs {
            root: root.clone(),
            path: path.into(),
            missing_ok,
        };
        self.call(self.api.virtx_client_remove_file, args)
    }

    /// Move what is at `from` in `root` to `to` in it, replacing what is at `to` if
    /// `overwrite`, and refused if not and something is there.
    pub(crate) fn rename_file(
        &self,
        root: &FileRoot,
        from: impl Into<String>,
        to: impl Into<String>,
        overwrite: bool,
    ) -> Call<()> {
        let args = FileMoveArgs {
            root: root.clone(),
            from: from.into(),
            to: to.into(),
            overwrite,
        };
        self.call(self.api.virtx_client_rename_file, args)
    }

    /// Copy what is at `from` in `root` to `to` in it, a directory with everything in it, as
    /// [`rename_file`](Client::rename_file) replaces what is at `to`.
    pub(crate) fn copy_file(
        &self,
        root: &FileRoot,
        from: impl Into<String>,
        to: impl Into<String>,
        overwrite: bool,
    ) -> Call<()> {
        let args = FileMoveArgs {
            root: root.clone(),
            from: from.into(),
            to: to.into(),
            overwrite,
        };
        self.call(self.api.virtx_client_copy_file, args)
    }

    /// Every sandbox the server holds.
    pub fn list_sandbox(&self) -> Call<Vec<Sandbox>> {
        let client = self.clone();
        let call: Call<Vec<SandboxInfo>> = Call::new(self.api, unsafe {
            (self.api.virtx_client_list_sandbox)(self.raw)
        });
        call.map(move |infos| {
            infos
                .into_iter()
                .map(|info| Sandbox::new(client.clone(), info))
                .collect()
        })
    }

    /// Make a sandbox as `args` says; see [`SandboxCreateArgs::new`]. Its ports are answered
    /// here only.
    ///
    /// The image has to be on the server already, pulled or built: this does not pull it, and a
    /// tag the server has no image by is refused with `404`.
    pub fn create_sandbox(&self, args: &SandboxCreateArgs) -> Call<Sandbox> {
        let client = self.clone();
        let call: Call<SandboxInfo> = self.call(self.api.virtx_client_create_sandbox, args);
        call.map(move |info| Sandbox::new(client.clone(), info))
    }

    /// The sandbox `info` describes, reached through this client, without asking the server: for
    /// one another module over the same library holds.
    pub fn sandbox(&self, info: SandboxInfo) -> Sandbox {
        Sandbox::new(self.clone(), info)
    }

    /// The sandbox `id`, without its ports.
    pub fn get_sandbox(&self, id: impl AsRef<str>) -> Call<Sandbox> {
        let args = json!({ "id": id.as_ref() });
        let client = self.clone();
        let call: Call<SandboxInfo> = self.call(self.api.virtx_client_get_sandbox, args);
        call.map(move |info| Sandbox::new(client.clone(), info))
    }

    /// Stop the sandbox `id`, end its jobs, and forget it, keeping its snapshot and volumes.
    pub(crate) fn remove_sandbox(&self, id: impl AsRef<str>) -> Call<()> {
        let args = json!({ "id": id.as_ref() });
        self.call(self.api.virtx_client_remove_sandbox, args)
    }

    /// Every job in the sandbox `id`.
    pub(crate) fn list_sandbox_job(&self, id: impl AsRef<str>) -> Call<Vec<SandboxJob>> {
        let sandbox = id.as_ref().to_owned();
        let args = json!({ "sandbox": id.as_ref() });
        let client = self.clone();
        let call: Call<Vec<SandboxJobInfo>> =
            self.call(self.api.virtx_client_list_sandbox_job, args);
        call.map(move |infos| {
            infos
                .into_iter()
                .map(|info| SandboxJob::new(client.clone(), sandbox.clone(), info))
                .collect()
        })
    }

    /// Start a process in the sandbox `id` as `args` says; see [`SandboxExecArgs::new`]. Its
    /// output and how it ended are [`sandbox_job_log`](Self::sandbox_job_log)'s, or
    /// [`StartedJob::log`].
    pub(crate) fn exec_sandbox(
        &self,
        id: impl AsRef<str>,
        args: &SandboxExecArgs,
    ) -> StartedJob<SandboxJob, SandboxJobEntry> {
        let sandbox = id.as_ref().to_owned();
        let args = json!({
            "sandbox": id.as_ref(),
            "cmd": args.cmd,
            "stdin": args.stdin,
            "timeout_ms": args.timeout_ms,
        });
        let client = self.clone();
        let call: Call<SandboxJobInfo> = self.call(self.api.virtx_client_exec_sandbox, args);
        StartedJob::new(
            call.map(move |info| SandboxJob::new(client.clone(), sandbox.clone(), info)),
            Box::new(|job: &SandboxJob| job.log(None)),
        )
    }

    /// The job `job` in the sandbox `id`.
    pub(crate) fn get_sandbox_job(
        &self,
        id: impl AsRef<str>,
        job: impl AsRef<str>,
    ) -> Call<SandboxJob> {
        let sandbox = id.as_ref().to_owned();
        let args = json!({ "sandbox": id.as_ref(), "job": job.as_ref() });
        let client = self.clone();
        let call: Call<SandboxJobInfo> = self.call(self.api.virtx_client_get_sandbox_job, args);
        call.map(move |info| SandboxJob::new(client.clone(), sandbox.clone(), info))
    }

    /// Cancel the job `job` in the sandbox `id` if it runs, killing its process, and forget it.
    pub(crate) fn remove_sandbox_job(&self, id: impl AsRef<str>, job: impl AsRef<str>) -> Call<()> {
        let args = json!({ "sandbox": id.as_ref(), "job": job.as_ref() });
        self.call(self.api.virtx_client_remove_sandbox_job, args)
    }

    /// Send `signal`, such as `SIGTERM`, to the process of the job `job` in the sandbox `id`
    /// and every process it started.
    pub(crate) fn signal_sandbox_job(
        &self,
        id: impl AsRef<str>,
        job: impl AsRef<str>,
        signal: impl AsRef<str>,
    ) -> Call<()> {
        let args =
            json!({ "sandbox": id.as_ref(), "job": job.as_ref(), "signal": signal.as_ref() });
        self.call(self.api.virtx_client_signal_sandbox_job, args)
    }

    /// Write `data` to the stdin of the process of the job `job` in the sandbox `id`, then
    /// close it if `eof`. Answers once it is written: a caller writes again only then, so the
    /// writes land in order.
    pub(crate) fn write_sandbox_job_stdin(
        &self,
        id: impl AsRef<str>,
        job: impl AsRef<str>,
        data: Option<impl Into<String>>,
        eof: bool,
    ) -> Call<()> {
        let args = json!({
            "sandbox": id.as_ref(),
            "job": job.as_ref(),
            "data": data.map(Into::into),
            "eof": eof,
        });
        self.call(self.api.virtx_client_write_sandbox_job_stdin, args)
    }

    /// The log of the job `job` in the sandbox `id`, its process's output, from its start or
    /// after its `after`th entry, until its `end` entry. Dropping it leaves the job as it is.
    pub(crate) fn sandbox_job_log(
        &self,
        id: impl AsRef<str>,
        job: impl AsRef<str>,
        after: Option<u64>,
    ) -> Events<LogEntry<SandboxJobEntry>> {
        let args = json!({ "sandbox": id.as_ref(), "job": job.as_ref(), "after": after });
        Events::new(
            self.api,
            self.start(self.api.virtx_client_sandbox_job_log, args),
        )
    }

    /// The image job `call` starts, its log read with a clone of this client.
    fn start_image_job(&self, call: Call<ImageJobInfo>) -> StartedJob<ImageJob, ImageJobEntry> {
        let client = self.clone();
        StartedJob::new(
            call.map(move |info| ImageJob::new(client.clone(), info)),
            Box::new(|job: &ImageJob| job.log(None)),
        )
    }

    /// `path` in `root`, as a files request names it.
    fn at(&self, root: &FileRoot, path: impl Into<String>) -> FilePathArgs {
        FilePathArgs {
            root: root.clone(),
            path: path.into(),
        }
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

    fn call<T: DeserializeOwned + 'static>(
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
    /// The last one ends the session, blocking until it has: as [`Client::close`], waited for.
    fn drop(&mut self) {
        let _ = Call::<()>::new(self.api, unsafe {
            (self.api.virtx_client_release)(self.raw)
        })
        .wait();
    }
}

/// `headers`, each name then its value, as `virtx_client_connect` takes them.
fn header_strings(
    headers: impl IntoIterator<Item = (impl AsRef<str>, impl AsRef<str>)>,
) -> Result<Vec<CString>, Error> {
    let mut strings = Vec::new();
    for (name, value) in headers {
        strings.push(c_string(name.as_ref())?);
        strings.push(c_string(value.as_ref())?);
    }
    Ok(strings)
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
