//! The typed client: each request `PROTOCOL.md` lists, as a call over a [`TransportClient`],
//! made by the module of what it names: [`image`](crate::image), [`snapshot`](crate::snapshot),
//! [`volume`](crate::volume) and [`sandbox`](crate::sandbox), and the files of a volume or a snapshot
//! over WebDAV.

use std::{io, path::PathBuf};

use bytes::Bytes;
use futures_util::Stream;
use http::HeaderMap;
use tokio::process::Command;

use crate::{
    Auth, Error,
    image::{
        self, Event, ImageBuildArgs, ImageDownloadOutput, ImageJob, ImageJobEntry,
        ImageListOutputItem, ImagePruneOutput, ImagePullArgs, ImagePushArgs, ImageUploadOutput,
        Recipe, RegistryAuth, oci,
    },
    sandbox::{
        self, Sandbox, SandboxCreateArgs, SandboxExecArgs, SandboxJob, SandboxJobEntry,
        SandboxSignalArgs, SandboxStdinArgs,
    },
    snapshot::{self, SnapshotCreateArgs, SnapshotListOutputItem},
    transport::{DirEntry, FileRoot, LogEntry, Metadata, TransportClient, webdav},
    volume::{
        self, MountSource, VolumeCreateArgs, VolumeListOutputItem, VolumeMountCreateArgs,
        VolumeMountListOutputItem,
    },
};

/// A client of one server, over stdio or HTTP.
pub struct Client {
    pub(crate) inner: TransportClient,
}

impl Client {
    /// Start `command` as the server, and talk to it over its stdin and stdout. Must be called
    /// inside a tokio runtime.
    pub fn spawn(command: impl Into<Command>) -> io::Result<Client> {
        let inner = TransportClient::spawn(command)?;
        Ok(Client { inner })
    }

    /// Talk to the server at the other end of `io`, one HTTP/2 connection as over a server's
    /// stdio. Under the `test-util` feature, for tests alone: a server served in the test's own
    /// process with [`serve_stdio`](crate::serve_stdio), over an in-memory pipe. Must be called
    /// inside a tokio runtime.
    #[cfg(feature = "test-util")]
    pub fn over_stream<T>(io: T) -> Client
    where
        T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin + 'static,
    {
        Client {
            inner: TransportClient::over(io),
        }
    }

    /// Reach the server at `url`, its base `http` or `https` URL, sending `auth`, the
    /// credentials it wants, and `headers`, whatever else it asks of each request, with every
    /// request. `Authorization` is `auth`'s, over one given as a header. Refused when a token
    /// would go in the clear: over `http` to any host but this machine.
    pub fn connect(url: &str, auth: Auth, headers: HeaderMap) -> Result<Client, Error> {
        let inner = TransportClient::connect(url, auth, headers)?;
        Ok(Client { inner })
    }

    /// Send `auth` with every request from now on, in place of what was given at
    /// [`connect`](Self::connect): a new token, when the server's are short-lived. A forwarder
    /// already open sends it with its next connection. Over stdio, where none is sent, nothing
    /// changes. Refused as at `connect`, the auth unchanged.
    pub fn set_auth(&self, auth: Auth) -> Result<(), Error> {
        self.inner.set_auth(auth)
    }

    /// Send `headers` with every request from now on, in place of those given at
    /// [`connect`](Self::connect). A forwarder already open sends them with its next
    /// connection. Over stdio, where none are sent, nothing changes.
    pub fn set_headers(&self, headers: HeaderMap) {
        self.inner.set_headers(headers);
    }

    /// Start building `recipe`, to point `reference` at what it builds. Its log is
    /// [`image_job_log`](Self::image_job_log)'s.
    pub async fn build_image(
        &self,
        recipe: Recipe,
        reference: impl Into<String>,
    ) -> Result<ImageJob, Error> {
        let args = ImageBuildArgs {
            recipe,
            reference: reference.into(),
        };
        image::client::build(&self.inner, &args).await
    }

    /// Start pulling `reference` from its registry, to point it at what it pulls; without
    /// `auth` the server pulls anonymously. Its log is [`image_job_log`](Self::image_job_log)'s.
    pub async fn pull_image(
        &self,
        reference: impl Into<String>,
        auth: Option<RegistryAuth>,
    ) -> Result<ImageJob, Error> {
        let args = ImagePullArgs {
            reference: reference.into(),
            auth,
        };
        image::client::pull(&self.inner, &args).await
    }

    /// Start pushing the image `reference` points at to the registry `reference` names;
    /// without `auth` the server pushes anonymously. Its log is
    /// [`image_job_log`](Self::image_job_log)'s.
    pub async fn push_image(
        &self,
        reference: impl Into<String>,
        auth: Option<RegistryAuth>,
    ) -> Result<ImageJob, Error> {
        let args = ImagePushArgs {
            reference: reference.into(),
            auth,
        };
        image::client::push(&self.inner, &args).await
    }

    /// Every image job the server holds.
    pub async fn list_image_job(&self) -> Result<Vec<ImageJob>, Error> {
        image::client::list_job(&self.inner).await
    }

    /// The image job `id`.
    pub async fn get_image_job(&self, id: impl AsRef<str>) -> Result<ImageJob, Error> {
        image::client::get_job(&self.inner, id.as_ref()).await
    }

    /// Cancel the image job `id` if it runs, and forget it.
    pub async fn remove_image_job(&self, id: impl AsRef<str>) -> Result<(), Error> {
        image::client::remove_job(&self.inner, id.as_ref()).await
    }

    /// The log of the image job `id`, from its start or after its `after`th entry, until its
    /// `end` entry, or [`Error::Job`] in its place if the job failed. Dropping it leaves the
    /// job as it is.
    pub fn image_job_log(
        &self,
        id: &str,
        after: Option<u64>,
    ) -> impl Stream<Item = Result<LogEntry<ImageJobEntry>, Error>> + Unpin + Send + use<'_> {
        Box::pin(image::client::job_log(&self.inner, id, after))
    }

    /// Every tag on the server's images, as `name:tag`, with the digest it points at.
    pub async fn list_image(&self) -> Result<Vec<ImageListOutputItem>, Error> {
        oci::list(&self.inner).await
    }

    /// Point the tag `target` (`name:tag`) at the image `source` names (`name:tag` or
    /// `name@algorithm:hex`), moving it if it is in use.
    pub async fn tag_image(
        &self,
        source: impl AsRef<str>,
        target: impl AsRef<str>,
    ) -> Result<(), Error> {
        oci::tag(&self.inner, source.as_ref(), target.as_ref()).await
    }

    /// Remove the tag `reference` (`name:tag`), leaving the image it points at.
    pub async fn untag_image(&self, reference: impl AsRef<str>) -> Result<(), Error> {
        oci::untag(&self.inner, reference.as_ref()).await
    }

    /// Remove the image `reference` names (`name:tag` or `name@algorithm:hex`), and every tag on
    /// it in that name.
    pub async fn remove_image(&self, reference: impl AsRef<str>) -> Result<(), Error> {
        oci::remove(&self.inner, reference.as_ref()).await
    }

    /// Write the image `reference` names to `path`, as an OCI image layout in a tar archive,
    /// replacing what is there once all of it is written. The stream is a line of log a blob,
    /// then what it wrote as its last item, an [`Event::Output`] or the error it failed with.
    /// Dropping it drops the download, and writes nothing.
    pub fn download_image(
        &self,
        reference: impl Into<String>,
        path: impl Into<PathBuf>,
    ) -> impl Stream<Item = Result<Event<ImageDownloadOutput, String>, Error>> + Unpin + Send + '_
    {
        Box::pin(oci::download(&self.inner, reference.into(), path.into()))
    }

    /// Upload the image in the archive at `path`, an OCI image layout in a tar archive, and
    /// point the tag `reference` (`name:tag`) at it: the archive's only image, or the one it
    /// names by that tag. Blobs the server has already are skipped. The stream is a line of log
    /// a blob, then what it uploaded as its last item, an [`Event::Output`] or the error it
    /// failed with. Dropping it drops the upload, and moves no tag.
    pub fn upload_image(
        &self,
        path: impl Into<PathBuf>,
        reference: impl Into<String>,
    ) -> impl Stream<Item = Result<Event<ImageUploadOutput, String>, Error>> + Unpin + Send + '_
    {
        Box::pin(oci::upload(&self.inner, path.into(), reference.into()))
    }

    /// Delete each image no tag points at and no sandbox uses, and what only such images held.
    pub async fn prune_image(&self) -> Result<ImagePruneOutput, Error> {
        image::client::prune(&self.inner).await
    }

    /// Every snapshot the server holds.
    pub async fn list_snapshot(&self) -> Result<Vec<SnapshotListOutputItem>, Error> {
        snapshot::client::list(&self.inner).await
    }

    /// Keep a new, empty upper layer under `name`, holding at most `disk_gib` GiB; without
    /// either the server picks it.
    pub async fn create_snapshot(
        &self,
        name: Option<impl Into<String>>,
        disk_gib: Option<u32>,
    ) -> Result<SnapshotListOutputItem, Error> {
        let args = SnapshotCreateArgs {
            name: name.map(Into::into),
            disk_gib,
        };
        snapshot::client::create(&self.inner, &args).await
    }

    /// The snapshot `name`.
    pub async fn get_snapshot(
        &self,
        name: impl AsRef<str>,
    ) -> Result<SnapshotListOutputItem, Error> {
        snapshot::client::get(&self.inner, name.as_ref()).await
    }

    /// Delete the snapshot `name` and its upper layer.
    pub async fn remove_snapshot(&self, name: impl AsRef<str>) -> Result<(), Error> {
        snapshot::client::remove(&self.inner, name.as_ref()).await
    }

    /// Every volume the server holds.
    pub async fn list_volume(&self) -> Result<Vec<VolumeListOutputItem>, Error> {
        volume::client::list(&self.inner).await
    }

    /// Keep a new, empty volume under `name`; without one the server picks it.
    pub async fn create_volume(
        &self,
        name: Option<impl Into<String>>,
    ) -> Result<VolumeListOutputItem, Error> {
        let args = VolumeCreateArgs {
            name: name.map(Into::into),
        };
        volume::client::create(&self.inner, &args).await
    }

    /// The volume `name`.
    pub async fn get_volume(&self, name: impl AsRef<str>) -> Result<VolumeListOutputItem, Error> {
        volume::client::get(&self.inner, name.as_ref()).await
    }

    /// Delete the volume `name` and everything in it.
    pub async fn remove_volume(&self, name: impl AsRef<str>) -> Result<(), Error> {
        volume::client::remove(&self.inner, name.as_ref()).await
    }

    /// The mounts in `volume`, without their credentials.
    pub async fn list_volume_mount(
        &self,
        volume: impl AsRef<str>,
    ) -> Result<Vec<VolumeMountListOutputItem>, Error> {
        volume::client::list_mount(&self.inner, volume.as_ref()).await
    }

    /// Mount `source` at `path` in `volume`, a directory the mount creates.
    pub async fn create_volume_mount(
        &self,
        volume: impl AsRef<str>,
        path: impl AsRef<str>,
        source: MountSource,
    ) -> Result<VolumeMountListOutputItem, Error> {
        let args = VolumeMountCreateArgs { source };
        volume::client::create_mount(&self.inner, volume.as_ref(), path.as_ref(), &args).await
    }

    /// Take away the mount at `path` in `volume`, leaving its source as it is.
    pub async fn remove_volume_mount(
        &self,
        volume: impl AsRef<str>,
        path: impl AsRef<str>,
    ) -> Result<(), Error> {
        volume::client::remove_mount(&self.inner, volume.as_ref(), path.as_ref()).await
    }

    /// What is at `path` in `root`.
    pub async fn stat_file(&self, root: &FileRoot, path: &str) -> Result<Metadata, Error> {
        webdav::client::stat(&self.inner, root, path).await
    }

    /// Whether anything is at `path` in `root`.
    pub async fn exists(&self, root: &FileRoot, path: &str) -> Result<bool, Error> {
        webdav::client::exists(&self.inner, root, path).await
    }

    /// Whether a file is at `path` in `root`.
    pub async fn is_file(&self, root: &FileRoot, path: &str) -> Result<bool, Error> {
        webdav::client::is_file(&self.inner, root, path).await
    }

    /// Whether a directory is at `path` in `root`.
    pub async fn is_dir(&self, root: &FileRoot, path: &str) -> Result<bool, Error> {
        webdav::client::is_dir(&self.inner, root, path).await
    }

    /// The entries of the directory at `path` in `root`.
    pub async fn read_dir(&self, root: &FileRoot, path: &str) -> Result<Vec<DirEntry>, Error> {
        webdav::client::read_dir(&self.inner, root, path).await
    }

    /// The file at `path` in `root`, from `offset`, `length` bytes of it or all that is left.
    pub async fn read_file(
        &self,
        root: &FileRoot,
        path: &str,
        offset: u64,
        length: Option<u64>,
    ) -> Result<Bytes, Error> {
        webdav::client::read(&self.inner, root, path, offset, length).await
    }

    /// The whole file at `path` in `root` as text, which it has to be: UTF-8.
    pub async fn read_text(&self, root: &FileRoot, path: &str) -> Result<String, Error> {
        webdav::client::read_text(&self.inner, root, path).await
    }

    /// Write `content` as the whole file at `path` in `root`, creating it if it does not
    /// exist. Its parent directory must exist. With `if_match`, the `etag` a [`Metadata`] of
    /// the file gave, the write goes only while the file still has it, and is refused with a
    /// `412` otherwise.
    pub async fn write_file(
        &self,
        root: &FileRoot,
        path: &str,
        content: impl Into<Bytes>,
        if_match: Option<&str>,
    ) -> Result<(), Error> {
        webdav::client::write(&self.inner, root, path, content, if_match).await
    }

    /// Create the directory at `path` in `root`, whose parent must exist.
    pub async fn create_dir(&self, root: &FileRoot, path: &str) -> Result<(), Error> {
        webdav::client::create_dir(&self.inner, root, path).await
    }

    /// Have the directory at `path` in `root`, as `mkdir -p`: its parents are created as
    /// needed, and one there already is left as it is. Refused with a `405` when a file is at
    /// `path`.
    pub async fn create_dir_all(&self, root: &FileRoot, path: &str) -> Result<(), Error> {
        webdav::client::create_dir_all(&self.inner, root, path).await
    }

    /// Remove what is at `path` in `root`: a directory with everything in it. With
    /// `missing_ok`, nothing being there is fine, as `rm -f`; without it, a `404`.
    pub async fn remove_file(
        &self,
        root: &FileRoot,
        path: &str,
        missing_ok: bool,
    ) -> Result<(), Error> {
        webdav::client::remove(&self.inner, root, path, missing_ok).await
    }

    /// Move what is at `from` in `root` to `to` in it, replacing what is at `to` if
    /// `overwrite`, and refused if not and something is there.
    pub async fn rename_file(
        &self,
        root: &FileRoot,
        from: &str,
        to: &str,
        overwrite: bool,
    ) -> Result<(), Error> {
        webdav::client::rename(&self.inner, root, from, to, overwrite).await
    }

    /// Copy what is at `from` in `root` to `to` in it, a directory with everything in it, as
    /// [`rename_file`](Self::rename_file) replaces what is at `to`.
    pub async fn copy_file(
        &self,
        root: &FileRoot,
        from: &str,
        to: &str,
        overwrite: bool,
    ) -> Result<(), Error> {
        webdav::client::copy(&self.inner, root, from, to, overwrite).await
    }

    /// Every sandbox the server holds.
    pub async fn list_sandbox(&self) -> Result<Vec<Sandbox>, Error> {
        sandbox::client::list(&self.inner).await
    }

    /// Make a sandbox as `args` says; see [`SandboxCreateArgs::new`]. Its ports are answered
    /// here only.
    pub async fn create_sandbox(&self, args: &SandboxCreateArgs) -> Result<Sandbox, Error> {
        sandbox::client::create(&self.inner, args).await
    }

    /// The sandbox `id`, without its ports.
    pub async fn get_sandbox(&self, id: impl AsRef<str>) -> Result<Sandbox, Error> {
        sandbox::client::get(&self.inner, id.as_ref()).await
    }

    /// Stop the sandbox `id`, end its jobs, and forget it, keeping its snapshot and volumes.
    pub async fn remove_sandbox(&self, id: impl AsRef<str>) -> Result<(), Error> {
        sandbox::client::remove(&self.inner, id.as_ref()).await
    }

    /// Every job in the sandbox `id`.
    pub async fn list_sandbox_job(&self, id: impl AsRef<str>) -> Result<Vec<SandboxJob>, Error> {
        sandbox::client::list_job(&self.inner, id.as_ref()).await
    }

    /// Start a process in the sandbox `id` as `args` says; see [`SandboxExecArgs::new`]. Its
    /// output and how it ended are [`sandbox_job_log`](Self::sandbox_job_log)'s.
    pub async fn exec_sandbox(
        &self,
        id: impl AsRef<str>,
        args: &SandboxExecArgs,
    ) -> Result<SandboxJob, Error> {
        sandbox::client::exec(&self.inner, id.as_ref(), args).await
    }

    /// The job `job` in the sandbox `id`.
    pub async fn get_sandbox_job(
        &self,
        id: impl AsRef<str>,
        job: impl AsRef<str>,
    ) -> Result<SandboxJob, Error> {
        sandbox::client::get_job(&self.inner, id.as_ref(), job.as_ref()).await
    }

    /// Cancel the job `job` in the sandbox `id` if it runs, killing its process, and forget it.
    pub async fn remove_sandbox_job(
        &self,
        id: impl AsRef<str>,
        job: impl AsRef<str>,
    ) -> Result<(), Error> {
        sandbox::client::remove_job(&self.inner, id.as_ref(), job.as_ref()).await
    }

    /// Send `signal`, such as `SIGTERM`, to the process of the job `job` in the sandbox `id`
    /// and every process it started.
    pub async fn signal_sandbox_job(
        &self,
        id: impl AsRef<str>,
        job: impl AsRef<str>,
        signal: impl Into<String>,
    ) -> Result<(), Error> {
        let args = SandboxSignalArgs {
            signal: signal.into(),
        };
        sandbox::client::signal_job(&self.inner, id.as_ref(), job.as_ref(), &args).await
    }

    /// Write `data` to the stdin of the process of the job `job` in the sandbox `id`, then
    /// close it if `eof`. Answers once it is written: a caller writes again only then, so the
    /// writes land in order.
    pub async fn write_sandbox_job_stdin(
        &self,
        id: impl AsRef<str>,
        job: impl AsRef<str>,
        data: Option<impl Into<String>>,
        eof: bool,
    ) -> Result<(), Error> {
        let args = SandboxStdinArgs {
            data: data.map(Into::into),
            eof,
        };
        sandbox::client::write_job_stdin(&self.inner, id.as_ref(), job.as_ref(), &args).await
    }

    /// The log of the job `job` in the sandbox `id`, its process's output, from its start or
    /// after its `after`th entry, until its `end` entry. Dropping it leaves the job as it is.
    pub fn sandbox_job_log(
        &self,
        id: &str,
        job: &str,
        after: Option<u64>,
    ) -> impl Stream<Item = Result<LogEntry<SandboxJobEntry>, Error>> + Unpin + Send + use<'_> {
        Box::pin(sandbox::client::job_log(&self.inner, id, job, after))
    }

    /// End the session; see [`TransportClient::close`].
    pub async fn close(self) {
        self.inner.close().await;
    }
}

#[cfg(feature = "ffi")]
pub(crate) mod ffi {
    //! [`Client`] behind the C ABI: opening one, sharing it by reference count, and its methods,
    //! each of which starts the request and hands C its [`VirtxFuture`] or [`VirtxStream`].

    use std::{
        ffi::{CStr, c_char, c_int},
        slice,
    };

    use bytes::Bytes;
    use futures_util::StreamExt;
    use http::{HeaderMap, HeaderName, HeaderValue};
    use serde::Deserialize;
    use tokio::process::Command;

    use super::Client;
    use crate::{
        Auth, Error,
        ffi::{
            Runtime, VIRTX_OK, VirtxBytes, VirtxFuture, VirtxStream, encode, failure, json,
            release, retain, share, write,
        },
        image::{
            ImageBuildArgs, ImageDownloadArgs, ImagePullArgs, ImagePushArgs, ImageRemoveArgs,
            ImageTagArgs, ImageUntagArgs, ImageUploadArgs,
        },
        sandbox::{SandboxCreateArgs, SandboxExecArgs},
        snapshot::SnapshotCreateArgs,
        transport::{FileMoveArgs, FilePathArgs, FileReadArgs, FileRemoveArgs, FileWriteArgs},
        volume::{MountSource, VolumeCreateArgs},
    };

    /// What names a snapshot or a volume: `{"name": "..."}`.
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Name {
        name: String,
    }

    /// What names a sandbox or an image job: `{"id": "..."}`.
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Id {
        id: String,
    }

    /// What reading an image job's log takes: `{"id": "...", "after": n}`.
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct ImageJobLogArgs {
        id: String,
        after: Option<u64>,
    }

    /// What names a volume's mounts, or one of them, and what mounting one takes:
    /// `{"volume": "...", "path": "...", "source": {...}}`.
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Mount {
        volume: String,
        #[serde(default)]
        path: String,
        source: Option<MountSource>,
    }

    /// What starting a process takes: its sandbox beside what `SandboxExecArgs` holds.
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Exec {
        sandbox: String,
        cmd: Vec<String>,
        #[serde(default)]
        stdin: bool,
        timeout_ms: Option<u64>,
    }

    /// What names a sandbox's jobs, or one of them, and what each request to one takes beside:
    /// `{"sandbox": "...", "job": "...", "signal": "...", "data": "...", "eof": bool,
    /// "after": n}`.
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Job {
        sandbox: String,
        job: Option<String>,
        signal: Option<String>,
        data: Option<String>,
        #[serde(default)]
        eof: bool,
        after: Option<u64>,
    }

    /// The argument that names no `field`, which the call takes.
    fn missing(field: &str) -> Error {
        Error::Client(format!("missing field `{field}`"))
    }

    /// A `Client`, as C holds it.
    pub struct VirtxClient(pub(crate) Runtime<Client>);

    /// Start `program` with `args_len` arguments from `args` as the server, over its stdin and
    /// stdout, and write the client, held once, to `client`. A null `program` is `virtx-stdio`,
    /// the server of this library's version, fetched first if this host has none -- see
    /// `virtx_ensure_virtx`, which does that fetch alone.
    ///
    /// # Safety
    ///
    /// `program` is null or NUL-terminated UTF-8, as each of the `args_len` pointers in `args`
    /// is. `client` is writable. `error` is null or writable.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_spawn(
        program: *const c_char,
        args: *const *const c_char,
        args_len: usize,
        client: *mut *mut VirtxClient,
        error: *mut VirtxBytes,
    ) -> c_int {
        let opened = (|| {
            let program = if program.is_null() {
                None
            } else {
                Some(unsafe { utf8(program) }?.to_string())
            };
            let args = (0..args_len)
                .map(|i| unsafe { utf8(*args.add(i)) }.map(str::to_string))
                .collect::<Result<Vec<_>, _>>()?;
            Runtime::open(|| {
                let mut command = match program {
                    Some(program) => Command::new(program),
                    // On the runtime being opened, which this thread is not a worker of.
                    None => Command::new(
                        tokio::runtime::Handle::current().block_on(crate::ensure_virtx())?,
                    ),
                };
                command.args(args);
                Client::spawn(command)
                    .map_err(|e| Error::Client(format!("starting the server: {e}")))
            })
        })();
        unsafe { share(opened.map(VirtxClient), client, error) }
    }

    /// Fetch `virtx-stdio`, the server of this library's version, into this host's cache if it
    /// is not there, and write its path, as a JSON string, to `out`: what `virtx_client_spawn`
    /// given a null `program` starts. Blocks for the fetch. `VIRTX_OK`, or `VIRTX_ERROR` with
    /// why written to `out`.
    ///
    /// The server is kept one directory per version, so a library that is upgraded fetches
    /// its own. `$VIRTX_STDIO` names a server to use as it is; `$VIRTX_STDIO_VERSION` another
    /// version; `$VIRTX_DIST_URL` where releases are fetched from; `$VIRTX_HOME` the cache.
    ///
    /// # Safety
    ///
    /// `out` is writable.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_ensure_virtx(out: *mut VirtxBytes) -> c_int {
        let fetched = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| Error::Client(format!("starting a runtime: {e}")))
            .and_then(|runtime| runtime.block_on(crate::ensure_virtx()))
            .map(|path| path.to_string_lossy().into_owned());
        let (status, bytes) = encode(fetched);
        unsafe { write(out, bytes) };
        status
    }

    /// `ptr` as a `&str`.
    ///
    /// # Safety
    ///
    /// `ptr` is NUL-terminated UTF-8, kept for `'a`.
    unsafe fn utf8<'a>(ptr: *const c_char) -> Result<&'a str, Error> {
        unsafe { CStr::from_ptr(ptr) }
            .to_str()
            .map_err(|e| Error::Client(e.to_string()))
    }

    /// The `headers_len` headers in `headers`, each its name then its value, so `2 *
    /// headers_len` strings.
    ///
    /// # Safety
    ///
    /// Each of the `2 * headers_len` pointers in `headers` is NUL-terminated UTF-8.
    unsafe fn header_map(
        headers: *const *const c_char,
        headers_len: usize,
    ) -> Result<HeaderMap, Error> {
        let mut map = HeaderMap::with_capacity(headers_len);
        for i in 0..headers_len {
            let name = unsafe { utf8(*headers.add(2 * i)) }?;
            let value = unsafe { utf8(*headers.add(2 * i + 1)) }?;
            map.insert(
                HeaderName::from_bytes(name.as_bytes())
                    .map_err(|e| Error::Client(format!("header {name}: {e}")))?,
                HeaderValue::from_str(value)
                    .map_err(|e| Error::Client(format!("header {name}: {e}")))?,
            );
        }
        Ok(map)
    }

    /// The `auth_len` bytes at `auth`: `{"type": "bearer", "token": "..."}`, or null for none.
    ///
    /// # Safety
    ///
    /// `auth` is null, or `auth_len` readable bytes.
    unsafe fn auth(auth: *const u8, auth_len: usize) -> Result<Auth, Error> {
        if auth.is_null() || auth_len == 0 {
            return Ok(Auth::None);
        }
        unsafe { json(auth, auth_len) }
    }

    /// Reach the server at `url`, its base `http` or `https` URL (every endpoint is under it), sending
    /// `auth`, the credentials it wants as the `auth_len` bytes of `{"type": "bearer", "token":
    /// "..."}` or null for none, and the `headers_len` headers in `headers` -- each its name
    /// then its value, so `2 * headers_len` strings -- with every request, and write the
    /// client, held once, to `client`. The headers are whatever else the server asks of each
    /// request; `Authorization` is `auth`'s. Refused when a token would go in the clear: over
    /// `http` to any host but this machine.
    ///
    /// # Safety
    ///
    /// `url` and each of the `2 * headers_len` pointers in `headers` are NUL-terminated UTF-8.
    /// `auth` is null or `auth_len` readable bytes. `client` is writable. `error` is null or
    /// writable.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_connect(
        url: *const c_char,
        auth: *const u8,
        auth_len: usize,
        headers: *const *const c_char,
        headers_len: usize,
        client: *mut *mut VirtxClient,
        error: *mut VirtxBytes,
    ) -> c_int {
        let opened = (|| {
            let url = unsafe { utf8(url) }?;
            let auth = unsafe { self::auth(auth, auth_len) }?;
            let map = unsafe { header_map(headers, headers_len) }?;
            Runtime::open(|| Client::connect(url, auth, map))
        })();
        unsafe { share(opened.map(VirtxClient), client, error) }
    }

    /// Send `auth`, as `virtx_client_connect` takes it, with every request from now on, in
    /// place of what was sent until now: a new token, when the server's are short-lived. A
    /// forwarder already open sends it with its next connection. Over stdio, where none is
    /// sent, nothing changes. Answers `VIRTX_OK`, or `VIRTX_ERROR` with why it is refused
    /// written to `error`, the auth unchanged.
    ///
    /// # Safety
    ///
    /// `client` is one `virtx_client_spawn` or `virtx_client_connect` wrote, still held. `auth`
    /// is null or `auth_len` readable bytes. `error` is null or writable.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_set_auth(
        client: *mut VirtxClient,
        auth: *const u8,
        auth_len: usize,
        error: *mut VirtxBytes,
    ) -> c_int {
        let set = unsafe { self::auth(auth, auth_len) }
            .and_then(|auth| unsafe { &(*client).0 }.value().set_auth(auth));
        match set {
            Ok(()) => VIRTX_OK,
            Err(failed) => {
                let (status, message) = failure(failed);
                unsafe { write(error, message) };
                status
            }
        }
    }

    /// Send the `headers_len` headers in `headers`, as `virtx_client_connect` takes them, with
    /// every request from now on, in place of those sent until now. A forwarder already open
    /// sends them with its next connection. Over stdio, where none are sent, nothing changes.
    /// Answers `VIRTX_OK`, or `VIRTX_ERROR` with why a header cannot be sent written to
    /// `error`, the headers unchanged.
    ///
    /// # Safety
    ///
    /// `client` is one `virtx_client_spawn` or `virtx_client_connect` wrote, still held. Each
    /// of the `2 * headers_len` pointers in `headers` is NUL-terminated UTF-8. `error` is null
    /// or writable.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_set_headers(
        client: *mut VirtxClient,
        headers: *const *const c_char,
        headers_len: usize,
        error: *mut VirtxBytes,
    ) -> c_int {
        match unsafe { header_map(headers, headers_len) } {
            Ok(map) => {
                unsafe { &(*client).0 }.value().set_headers(map);
                VIRTX_OK
            }
            Err(failed) => {
                let (status, message) = failure(failed);
                unsafe { write(error, message) };
                status
            }
        }
    }

    /// Hold `client` once more, for another holder to release.
    ///
    /// # Safety
    ///
    /// `client` is one `virtx_client_spawn` or `virtx_client_connect` wrote, still held.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_retain(client: *mut VirtxClient) {
        unsafe { retain(client) };
    }

    /// Let go of `client`. The last holder's release answers every request not yet answered
    /// with `VIRTX_ERROR`, ends the session, and frees it; its futures and streams stay to be
    /// polled and freed. Null does nothing.
    ///
    /// # Safety
    ///
    /// `client` is null, or held by the caller, who does not use it again. Not called from a
    /// `wake`, which runs on the runtime this may stop.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_release(client: *mut VirtxClient) {
        if let Some(VirtxClient(runtime)) = unsafe { release(client) } {
            runtime.close(Client::close);
        }
    }

    /// Starts building `recipe`, to point `reference` (`name:tag`) at it, given
    /// `{"recipe": {"v": 1, "base": "alpine:3.20", "steps": [{"run": "..."}]}, "reference": "name:tag"}`.
    /// Answers with the job, as `{"id": "...", "kind": "build", "reference": "name:tag",
    /// "running": bool, "digest": "algorithm:hex", "error": "..."}`, `digest` once it has
    /// succeeded and `error` once it has failed. Its log is `virtx_client_image_job_log`'s.
    ///
    /// # Safety
    ///
    /// `client` is as in `virtx_client_list_snapshot`. `args` is `args_len` readable bytes, not
    /// read after this returns.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_build_image(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxFuture {
        let args = unsafe { json::<ImageBuildArgs>(args, args_len) };
        unsafe { &(*client).0 }.future(|client| async move {
            let ImageBuildArgs { recipe, reference } = args?;
            client.build_image(recipe, reference).await
        })
    }

    /// Starts pulling `reference` from its registry, to point it at what it pulls, given
    /// `{"reference": "alpine:3.20", "auth": {...}}`, the optional auth as
    /// `{"type": "basic", "username": "...", "password": "..."}` or
    /// `{"type": "bearer", "token": "..."}`. Answers with the job, as
    /// `virtx_client_build_image` does, its `kind` `"pull"`.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_pull_image(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxFuture {
        let args = unsafe { json::<ImagePullArgs>(args, args_len) };
        unsafe { &(*client).0 }.future(|client| async move {
            let ImagePullArgs { reference, auth } = args?;
            client.pull_image(reference, auth).await
        })
    }

    /// Starts pushing the image `reference` points at to the registry `reference` names, given
    /// `{"reference": "ghcr.io/owner/name:tag", "auth": {...}}`, the optional auth as in
    /// `virtx_client_pull_image`. Answers with the job, as `virtx_client_build_image` does, its
    /// `kind` `"push"`.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_push_image(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxFuture {
        let args = unsafe { json::<ImagePushArgs>(args, args_len) };
        unsafe { &(*client).0 }.future(|client| async move {
            let ImagePushArgs { reference, auth } = args?;
            client.push_image(reference, auth).await
        })
    }

    /// Answers with every image job, each as `virtx_client_build_image` answers one.
    ///
    /// # Safety
    ///
    /// `client` is as in `virtx_client_list_snapshot`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_list_image_job(
        client: *mut VirtxClient,
    ) -> *mut VirtxFuture {
        unsafe { &(*client).0 }.future(|client| async move { client.list_image_job().await })
    }

    /// Answers with the image job `id`, given `{"id": "..."}`, as `virtx_client_build_image`
    /// answers one.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_get_image_job(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxFuture {
        let args = unsafe { json::<Id>(args, args_len) };
        unsafe { &(*client).0 }.future(|client| async move { client.get_image_job(args?.id).await })
    }

    /// Cancels the image job `id` if it runs, and forgets it, given `{"id": "..."}`. Answers
    /// with `null`.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_remove_image_job(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxFuture {
        let args = unsafe { json::<Id>(args, args_len) };
        unsafe { &(*client).0 }
            .future(|client| async move { client.remove_image_job(args?.id).await })
    }

    /// The log of the image job `id`, given `{"id": "...", "after": n}`, `after` optional: from
    /// its start, or after its `after`th entry. Its items are the entries, each with its number
    /// beside it: `{"id": n, "log": "..."}`, `{"id": n, "truncated": {}}` once if the log
    /// reached the server's limit, then `{"id": n, "end": {"digest": "..."}}` last, or, if the
    /// job failed, its error in that entry's place. Freeing it leaves the job as it is.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_image_job_log(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxStream {
        let args = unsafe { json::<ImageJobLogArgs>(args, args_len) };
        unsafe { &(*client).0 }.stream(|client| {
            async_stream::stream! {
                let ImageJobLogArgs { id, after } = match args {
                    Ok(args) => args,
                    Err(error) => {
                        yield Err(error);
                        return;
                    }
                };
                let mut log = client.image_job_log(&id, after);
                while let Some(entry) = log.next().await {
                    yield entry;
                }
            }
        })
    }

    /// Answers with every tag on the server's images, as
    /// `[{"reference": "name:tag", "digest": "algorithm:hex"}, ...]`.
    ///
    /// # Safety
    ///
    /// `client` is as in `virtx_client_list_snapshot`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_list_image(client: *mut VirtxClient) -> *mut VirtxFuture {
        unsafe { &(*client).0 }.future(|client| async move { client.list_image().await })
    }

    /// Points the tag `target` at the image `source` names, given
    /// `{"source": "name:tag" or "name@algorithm:hex", "target": "name:tag"}`. Answers with
    /// `null`.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_tag_image(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxFuture {
        let args = unsafe { json::<ImageTagArgs>(args, args_len) };
        unsafe { &(*client).0 }.future(|client| async move {
            let ImageTagArgs { source, target } = args?;
            client.tag_image(source, target).await
        })
    }

    /// Removes the tag `reference`, leaving its image, given `{"reference": "name:tag"}`.
    /// Answers with `null`.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_untag_image(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxFuture {
        let args = unsafe { json::<ImageUntagArgs>(args, args_len) };
        unsafe { &(*client).0 }
            .future(|client| async move { client.untag_image(args?.reference).await })
    }

    /// Removes the image `reference` names and every tag on it in that name, given
    /// `{"reference": "name:tag" or "name@algorithm:hex"}`. Answers with `null`.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_remove_image(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxFuture {
        let args = unsafe { json::<ImageRemoveArgs>(args, args_len) };
        unsafe { &(*client).0 }
            .future(|client| async move { client.remove_image(args?.reference).await })
    }

    /// Writes the image `reference` names to `path` as an OCI image layout in a tar archive,
    /// given `{"reference": "name:tag" or "name@algorithm:hex", "path": "..."}`. Its items are
    /// its log, a line a blob, as `{"progress": {"progress": n, "total": n, "content": "..."}}`
    /// (`total` and `content` each optional), then what it wrote, as
    /// `{"output": {"digest": "algorithm:hex"}}`.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_download_image(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxStream {
        let args = unsafe { json::<ImageDownloadArgs>(args, args_len) };
        unsafe { &(*client).0 }.stream(|client| {
            async_stream::stream! {
                let ImageDownloadArgs { reference, path } = match args {
                    Ok(args) => args,
                    Err(error) => {
                        yield Err(error);
                        return;
                    }
                };
                let mut download = client.download_image(reference, path);
                while let Some(event) = download.next().await {
                    yield event;
                }
            }
        })
    }

    /// Uploads the image in the OCI image layout archive at `path` and points the tag
    /// `reference` at it, given `{"path": "...", "reference": "name:tag"}`. Its items are its
    /// log, a line a blob, as `virtx_client_download_image`'s are, then what it uploaded, as
    /// `{"output": {"digest": "algorithm:hex"}}`.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_upload_image(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxStream {
        let args = unsafe { json::<ImageUploadArgs>(args, args_len) };
        unsafe { &(*client).0 }.stream(|client| {
            async_stream::stream! {
                let ImageUploadArgs { path, reference } = match args {
                    Ok(args) => args,
                    Err(error) => {
                        yield Err(error);
                        return;
                    }
                };
                let mut upload = client.upload_image(path, reference);
                while let Some(event) = upload.next().await {
                    yield event;
                }
            }
        })
    }

    /// Deletes each image no tag points at and no sandbox uses, and what only such images held.
    /// Answers with `{"images": ["algorithm:hex", ...], "reclaimed": n}`, the digests deleted
    /// and the bytes freed.
    ///
    /// # Safety
    ///
    /// `client` is as in `virtx_client_list_snapshot`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_prune_image(
        client: *mut VirtxClient,
    ) -> *mut VirtxFuture {
        unsafe { &(*client).0 }.future(|client| async move { client.prune_image().await })
    }

    /// Answers with the snapshots, as `[{"name": "...", "size": n, "in_use": false}, ...]`.
    ///
    /// Every value here is JSON. A request that takes arguments takes them as `args`, the
    /// `args_len` bytes of one object.
    ///
    /// # Safety
    ///
    /// `client` is one `virtx_client_spawn` or `virtx_client_connect` wrote, still held.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_list_snapshot(
        client: *mut VirtxClient,
    ) -> *mut VirtxFuture {
        unsafe { &(*client).0 }.future(|client| async move { client.list_snapshot().await })
    }

    /// Keeps a new, empty upper layer under `name`, holding at most `disk_gib` GiB, the server
    /// picking either one not given, given `{"name": "...", "disk_gib": 8}` or less. Answers
    /// with the snapshot, as `virtx_client_list_snapshot` answers each.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_create_snapshot(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxFuture {
        let args = unsafe { json::<SnapshotCreateArgs>(args, args_len) };
        unsafe { &(*client).0 }.future(|client| async move {
            let args = args?;
            client.create_snapshot(args.name, args.disk_gib).await
        })
    }

    /// Answers with the snapshot `name`, given `{"name": "..."}`, as
    /// `virtx_client_list_snapshot` answers each.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_get_snapshot(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxFuture {
        let args = unsafe { json::<Name>(args, args_len) };
        unsafe { &(*client).0 }
            .future(|client| async move { client.get_snapshot(args?.name).await })
    }

    /// Deletes the snapshot `name` and its upper layer, given `{"name": "..."}`. Answers with
    /// `null`.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_remove_snapshot(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxFuture {
        let args = unsafe { json::<Name>(args, args_len) };
        unsafe { &(*client).0 }
            .future(|client| async move { client.remove_snapshot(args?.name).await })
    }

    /// Answers with the volumes, as `[{"name": "...", "size": n}, ...]`.
    ///
    /// # Safety
    ///
    /// `client` is as in `virtx_client_list_snapshot`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_list_volume(
        client: *mut VirtxClient,
    ) -> *mut VirtxFuture {
        unsafe { &(*client).0 }.future(|client| async move { client.list_volume().await })
    }

    /// Keeps a new, empty volume under `name`, or a name the server picks if there is none,
    /// given `{"name": "..."}` or `{}`. Answers with the volume, as `virtx_client_list_volume`
    /// answers each.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_create_volume(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxFuture {
        let args = unsafe { json::<VolumeCreateArgs>(args, args_len) };
        unsafe { &(*client).0 }
            .future(|client| async move { client.create_volume(args?.name).await })
    }

    /// Answers with the volume `name`, given `{"name": "..."}`, as `virtx_client_list_volume`
    /// answers each.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_get_volume(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxFuture {
        let args = unsafe { json::<Name>(args, args_len) };
        unsafe { &(*client).0 }.future(|client| async move { client.get_volume(args?.name).await })
    }

    /// Deletes the volume `name` and everything in it, given `{"name": "..."}`. Answers with
    /// `null`.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_remove_volume(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxFuture {
        let args = unsafe { json::<Name>(args, args_len) };
        unsafe { &(*client).0 }
            .future(|client| async move { client.remove_volume(args?.name).await })
    }

    /// Given `{"volume": "..."}`, answers with the volume's mounts, as
    /// `[{"path": "...", "mount_type": "notion" or "gdrive" or "onedrive"}, ...]`.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_list_volume_mount(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxFuture {
        let args = unsafe { json::<Mount>(args, args_len) };
        unsafe { &(*client).0 }
            .future(|client| async move { client.list_volume_mount(args?.volume).await })
    }

    /// Given `{"volume": "...", "path": "...", "source": {...}}`, the source as
    /// `{"type": "notion", "api_key": "..."}`,
    /// `{"type": "gdrive", "client_id": "...", "client_secret": "...", "refresh_token": "..."}`
    /// or `{"type": "onedrive", "client_id": "...", "refresh_token": "..."}` (and
    /// `"client_secret"` if it has one), mounts it. Answers with the mount, as
    /// `virtx_client_list_volume_mount` answers each.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_create_volume_mount(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxFuture {
        let args = unsafe { json::<Mount>(args, args_len) };
        unsafe { &(*client).0 }.future(|client| async move {
            let Mount {
                volume,
                path,
                source,
            } = args?;
            let source = source.ok_or_else(|| missing("source"))?;
            client.create_volume_mount(volume, path, source).await
        })
    }

    /// Given `{"volume": "...", "path": "..."}`, takes the mount away. Answers with `null`.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_remove_volume_mount(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxFuture {
        let args = unsafe { json::<Mount>(args, args_len) };
        unsafe { &(*client).0 }.future(|client| async move {
            let Mount { volume, path, .. } = args?;
            client.remove_volume_mount(volume, path).await
        })
    }

    /// The files of a volume or a snapshot: what is at `path`, given
    /// `{"volume": "...", "path": "..."}`, `{"snapshot": "...", "path": "..."}` or
    /// `{"sandbox": "...", "path": "..."}`, the path
    /// relative to its root as `dir/file.txt`. Answers with
    /// `{"is_dir": bool, "size": n, "modified": n, "etag": "..."}`, `modified` in seconds since
    /// the Unix epoch and `etag` as the server spells it, each only if the server says.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_stat_file(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxFuture {
        let args = unsafe { json::<FilePathArgs>(args, args_len) };
        unsafe { &(*client).0 }.future(|client| async move {
            let FilePathArgs { root, path } = args?;
            client.stat_file(&root, &path).await
        })
    }

    /// Whether anything is at `path`, given as `virtx_client_stat_file` takes it. Answers
    /// with `true` or `false`.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_file_exists(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxFuture {
        let args = unsafe { json::<FilePathArgs>(args, args_len) };
        unsafe { &(*client).0 }.future(|client| async move {
            let FilePathArgs { root, path } = args?;
            client.exists(&root, &path).await
        })
    }

    /// Whether a file is at `path`, given as `virtx_client_stat_file` takes it. Answers with
    /// `true` or `false`.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_is_file(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxFuture {
        let args = unsafe { json::<FilePathArgs>(args, args_len) };
        unsafe { &(*client).0 }.future(|client| async move {
            let FilePathArgs { root, path } = args?;
            client.is_file(&root, &path).await
        })
    }

    /// Whether a directory is at `path`, given as `virtx_client_stat_file` takes it. Answers
    /// with `true` or `false`.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_is_dir(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxFuture {
        let args = unsafe { json::<FilePathArgs>(args, args_len) };
        unsafe { &(*client).0 }.future(|client| async move {
            let FilePathArgs { root, path } = args?;
            client.is_dir(&root, &path).await
        })
    }

    /// The entries of the directory at `path`, given as `virtx_client_stat_file` takes it.
    /// Answers with `[{"name": "...", "metadata": {...}}, ...]`, each metadata as
    /// `virtx_client_stat_file` answers it.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_read_dir(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxFuture {
        let args = unsafe { json::<FilePathArgs>(args, args_len) };
        unsafe { &(*client).0 }.future(|client| async move {
            let FilePathArgs { root, path } = args?;
            client.read_dir(&root, &path).await
        })
    }

    /// The file at `path`, given `{"volume": "...", "path": "...", "offset": n, "length": n}`,
    /// `offset` and `length` optional: from `offset`, `length` bytes of it or all that is
    /// left. Answers with the bytes read as they are, not as JSON.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_read_file(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxFuture {
        let args = unsafe { json::<FileReadArgs>(args, args_len) };
        unsafe { &(*client).0 }.future_raw(|client| async move {
            let FileReadArgs {
                root,
                path,
                offset,
                length,
            } = args?;
            client
                .read_file(&root, &path, offset, length)
                .await
                .map(|bytes| bytes.to_vec())
        })
    }

    /// The whole file at `path`, given as `virtx_client_stat_file` takes it, as text: refused
    /// unless it is UTF-8. Answers with the text as a JSON string.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_read_text(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxFuture {
        let args = unsafe { json::<FilePathArgs>(args, args_len) };
        unsafe { &(*client).0 }.future(|client| async move {
            let FilePathArgs { root, path } = args?;
            client.read_text(&root, &path).await
        })
    }

    /// Writes the `data_len` bytes at `data` as the whole file at `path`, given
    /// `{"volume": "...", "path": "...", "if_match": "..."}`, `if_match` optional: the `etag`
    /// `virtx_client_stat_file` gave, which the file has to have still, or the write is refused
    /// with a `412`. The file is created if it does not exist; its parent directory must exist.
    /// Answers with `null`.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`; `data` is `data_len` readable bytes, not read after this
    /// returns.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_write_file(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
        data: *const u8,
        data_len: usize,
    ) -> *mut VirtxFuture {
        let args = unsafe { json::<FileWriteArgs>(args, args_len) };
        let content = if data.is_null() {
            Bytes::new()
        } else {
            Bytes::copy_from_slice(unsafe { slice::from_raw_parts(data, data_len) })
        };
        unsafe { &(*client).0 }.future(|client| async move {
            let FileWriteArgs {
                root,
                path,
                if_match,
            } = args?;
            client
                .write_file(&root, &path, content, if_match.as_deref())
                .await
        })
    }

    /// Creates the directory at `path`, given as `virtx_client_stat_file` takes it, whose
    /// parent must exist. Answers with `null`.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_create_dir(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxFuture {
        let args = unsafe { json::<FilePathArgs>(args, args_len) };
        unsafe { &(*client).0 }.future(|client| async move {
            let FilePathArgs { root, path } = args?;
            client.create_dir(&root, &path).await
        })
    }

    /// Has the directory at `path`, given as `virtx_client_stat_file` takes it, as `mkdir -p`:
    /// its parents are created as needed, and one there already is left as it is. Refused with
    /// a `405` when a file is at `path`. Answers with `null`.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_create_dir_all(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxFuture {
        let args = unsafe { json::<FilePathArgs>(args, args_len) };
        unsafe { &(*client).0 }.future(|client| async move {
            let FilePathArgs { root, path } = args?;
            client.create_dir_all(&root, &path).await
        })
    }

    /// Removes what is at `path`, given `{"volume": "...", "path": "...", "missing_ok": bool}`,
    /// `missing_ok` optional and false without it: a directory with everything in it. With
    /// `missing_ok`, nothing being there is fine, as `rm -f`; without it, a `404`. Answers with
    /// `null`.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_remove_file(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxFuture {
        let args = unsafe { json::<FileRemoveArgs>(args, args_len) };
        unsafe { &(*client).0 }.future(|client| async move {
            let FileRemoveArgs {
                root,
                path,
                missing_ok,
            } = args?;
            client.remove_file(&root, &path, missing_ok).await
        })
    }

    /// Moves what is at `from` to `to` in the same volume or snapshot, given
    /// `{"volume": "...", "from": "...", "to": "...", "overwrite": bool}`, `overwrite` optional
    /// and false without it: whether to replace what is at `to`, which is otherwise refused.
    /// Answers with `null`.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_rename_file(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxFuture {
        let args = unsafe { json::<FileMoveArgs>(args, args_len) };
        unsafe { &(*client).0 }.future(|client| async move {
            let FileMoveArgs {
                root,
                from,
                to,
                overwrite,
            } = args?;
            client.rename_file(&root, &from, &to, overwrite).await
        })
    }

    /// Copies what is at `from` to `to`, a directory with everything in it, given as
    /// `virtx_client_rename_file` takes it. Answers with `null`.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_copy_file(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxFuture {
        let args = unsafe { json::<FileMoveArgs>(args, args_len) };
        unsafe { &(*client).0 }.future(|client| async move {
            let FileMoveArgs {
                root,
                from,
                to,
                overwrite,
            } = args?;
            client.copy_file(&root, &from, &to, overwrite).await
        })
    }

    /// Answers with the sandboxes, as
    /// `[{"id": "...", "image": "algorithm:hex", "snapshot": "...", "running": bool}, ...]`,
    /// `snapshot` only for one that has one.
    ///
    /// # Safety
    ///
    /// `client` is as in `virtx_client_list_snapshot`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_list_sandbox(
        client: *mut VirtxClient,
    ) -> *mut VirtxFuture {
        unsafe { &(*client).0 }.future(|client| async move { client.list_sandbox().await })
    }

    /// Makes a sandbox, given
    /// `{"image": {...}, "snapshot": "...", "network": bool, "ports": [n],
    /// "vcpus": n, "memory_mib": n, "gpu": bool, "gpu_memory_mib": n,
    /// "volumes": [{"volume": "...", "path": "/...", "readonly": bool}]}`, the image as
    /// `{"type": "tag", "tag": "name:tag"}` or
    /// `{"type": "digest", "digest": "algorithm:hex"}` and all but it optional. Answers with the
    /// sandbox as `virtx_client_list_sandbox` answers each, and
    /// `"ports": [{"port": n, "url": "..."}]` if it was given some: here only.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_create_sandbox(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxFuture {
        let args = unsafe { json::<SandboxCreateArgs>(args, args_len) };
        unsafe { &(*client).0 }.future(|client| async move { client.create_sandbox(&args?).await })
    }

    /// Answers with the sandbox `id`, given `{"id": "..."}`, as `virtx_client_list_sandbox`
    /// answers each.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_get_sandbox(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxFuture {
        let args = unsafe { json::<Id>(args, args_len) };
        unsafe { &(*client).0 }.future(|client| async move { client.get_sandbox(args?.id).await })
    }

    /// Stops the sandbox `id`, ends its jobs and forgets it, given `{"id": "..."}`. Answers
    /// with `null`.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_remove_sandbox(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxFuture {
        let args = unsafe { json::<Id>(args, args_len) };
        unsafe { &(*client).0 }
            .future(|client| async move { client.remove_sandbox(args?.id).await })
    }

    /// Given `{"sandbox": "..."}`, answers with its jobs, each as `virtx_client_exec_sandbox`
    /// answers one.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_list_sandbox_job(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxFuture {
        let args = unsafe { json::<Job>(args, args_len) };
        unsafe { &(*client).0 }
            .future(|client| async move { client.list_sandbox_job(args?.sandbox).await })
    }

    /// Starts a process, given `{"sandbox": "...", "cmd": ["..."], "stdin": bool,
    /// "timeout_ms": n}`, `stdin` and `timeout_ms` optional. Answers with its job, as
    /// `{"id": "...", "kind": "exec", "pid": n, "cmd": ["..."], "running": bool, "code": n}`,
    /// `code` once it has ended. Its output is `virtx_client_sandbox_job_log`'s.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_exec_sandbox(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxFuture {
        let args = unsafe { json::<Exec>(args, args_len) };
        unsafe { &(*client).0 }.future(|client| async move {
            let Exec {
                sandbox,
                cmd,
                stdin,
                timeout_ms,
            } = args?;
            let args = SandboxExecArgs {
                cmd,
                stdin,
                timeout_ms,
            };
            client.exec_sandbox(sandbox, &args).await
        })
    }

    /// Answers with the job `job` in the sandbox `sandbox`, given
    /// `{"sandbox": "...", "job": "..."}`, as `virtx_client_exec_sandbox` answers one.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_get_sandbox_job(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxFuture {
        let args = unsafe { json::<Job>(args, args_len) };
        unsafe { &(*client).0 }.future(|client| async move {
            let Job { sandbox, job, .. } = args?;
            client
                .get_sandbox_job(sandbox, job.ok_or_else(|| missing("job"))?)
                .await
        })
    }

    /// Cancels the job if it runs, killing its process, and forgets it, given
    /// `{"sandbox": "...", "job": "..."}`. Answers with `null`.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_remove_sandbox_job(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxFuture {
        let args = unsafe { json::<Job>(args, args_len) };
        unsafe { &(*client).0 }.future(|client| async move {
            let Job { sandbox, job, .. } = args?;
            client
                .remove_sandbox_job(sandbox, job.ok_or_else(|| missing("job"))?)
                .await
        })
    }

    /// Sends `signal`, a name such as `"SIGTERM"`, to the job's process and every process it
    /// started, given `{"sandbox": "...", "job": "...", "signal": "..."}`. Answers with `null`.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_signal_sandbox_job(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxFuture {
        let args = unsafe { json::<Job>(args, args_len) };
        unsafe { &(*client).0 }.future(|client| async move {
            let Job {
                sandbox,
                job,
                signal,
                ..
            } = args?;
            client
                .signal_sandbox_job(
                    sandbox,
                    job.ok_or_else(|| missing("job"))?,
                    signal.ok_or_else(|| missing("signal"))?,
                )
                .await
        })
    }

    /// Writes `data` to the stdin of the job's process, then closes it if `eof`, given
    /// `{"sandbox": "...", "job": "...", "data": "...", "eof": bool}`, `data` and `eof`
    /// optional. Answers with `null` once it is written: the next write is sent only then.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_write_sandbox_job_stdin(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxFuture {
        let args = unsafe { json::<Job>(args, args_len) };
        unsafe { &(*client).0 }.future(|client| async move {
            let Job {
                sandbox,
                job,
                data,
                eof,
                ..
            } = args?;
            client
                .write_sandbox_job_stdin(sandbox, job.ok_or_else(|| missing("job"))?, data, eof)
                .await
        })
    }

    /// The log of the job, given `{"sandbox": "...", "job": "...", "after": n}`, `after`
    /// optional: from its start, or after its `after`th entry. Its items are the entries, each
    /// with its number beside it: `{"id": n, "stdout": "..."}` or `{"id": n, "stderr": "..."}`,
    /// a line as it is written, `{"id": n, "truncated": {}}` once if the log reached the
    /// server's limit, then `{"id": n, "end": {"code": n}}` last. Freeing it leaves the job as
    /// it is.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_sandbox_job_log(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxStream {
        let args = unsafe { json::<Job>(args, args_len) };
        unsafe { &(*client).0 }.stream(|client| {
            async_stream::stream! {
                let (sandbox, job, after) = match args {
                    Ok(Job { sandbox, job: Some(job), after, .. }) => (sandbox, job, after),
                    Ok(_) => {
                        yield Err(missing("job"));
                        return;
                    }
                    Err(error) => {
                        yield Err(error);
                        return;
                    }
                };
                let mut log = client.sandbox_job_log(&sandbox, &job, after);
                while let Some(entry) = log.next().await {
                    yield entry;
                }
            }
        })
    }

    #[cfg(test)]
    mod tests {
        use std::ptr;

        use super::*;
        use crate::ffi::virtx_bytes_free;

        #[test]
        fn spawning_nothing_fails_with_a_message() {
            let mut client = ptr::null_mut();
            let mut error = VirtxBytes {
                data: ptr::null_mut(),
                len: 0,
            };
            let program = c"/nonexistent/virtx-server";
            let status = unsafe {
                virtx_client_spawn(program.as_ptr(), ptr::null(), 0, &mut client, &mut error)
            };
            assert_eq!(status, crate::ffi::VIRTX_ERROR);
            assert!(client.is_null());
            assert!(!error.data.is_null());
            unsafe { virtx_bytes_free(error) };
        }

        #[test]
        fn a_header_that_cannot_be_sent_is_refused() {
            let mut client = ptr::null_mut();
            let url = c"http://127.0.0.1:9/";
            let status = unsafe {
                virtx_client_connect(
                    url.as_ptr(),
                    ptr::null(),
                    0,
                    ptr::null(),
                    0,
                    &mut client,
                    ptr::null_mut(),
                )
            };
            assert_eq!(status, VIRTX_OK);
            let mut error = VirtxBytes {
                data: ptr::null_mut(),
                len: 0,
            };
            let headers = [c"not a name".as_ptr(), c"x".as_ptr()];
            let status =
                unsafe { virtx_client_set_headers(client, headers.as_ptr(), 1, &mut error) };
            assert_eq!(status, crate::ffi::VIRTX_ERROR);
            let message = unsafe { std::slice::from_raw_parts(error.data, error.len) };
            assert!(String::from_utf8_lossy(message).contains("not a name"));
            unsafe { virtx_bytes_free(error) };
            unsafe { virtx_client_release(client) };
        }

        #[test]
        fn an_auth_is_taken_as_json_and_refused_as_at_connect() {
            let mut client = ptr::null_mut();
            let url = c"http://127.0.0.1:9/";
            let bearer = br#"{"type": "bearer", "token": "t"}"#;
            let status = unsafe {
                virtx_client_connect(
                    url.as_ptr(),
                    bearer.as_ptr(),
                    bearer.len(),
                    ptr::null(),
                    0,
                    &mut client,
                    ptr::null_mut(),
                )
            };
            assert_eq!(status, VIRTX_OK);
            let mut error = VirtxBytes {
                data: ptr::null_mut(),
                len: 0,
            };
            let basic = br#"{"type": "basic", "username": "u", "password": "p"}"#;
            let status =
                unsafe { virtx_client_set_auth(client, basic.as_ptr(), basic.len(), &mut error) };
            assert_eq!(status, crate::ffi::VIRTX_ERROR);
            let message = unsafe { std::slice::from_raw_parts(error.data, error.len) };
            assert!(String::from_utf8_lossy(message).contains("basic"));
            unsafe { virtx_bytes_free(error) };
            unsafe { virtx_client_release(client) };

            let url = c"http://example.com/";
            let mut error = VirtxBytes {
                data: ptr::null_mut(),
                len: 0,
            };
            let status = unsafe {
                virtx_client_connect(
                    url.as_ptr(),
                    bearer.as_ptr(),
                    bearer.len(),
                    ptr::null(),
                    0,
                    &mut client,
                    &mut error,
                )
            };
            assert_eq!(status, crate::ffi::VIRTX_ERROR);
            let message = unsafe { std::slice::from_raw_parts(error.data, error.len) };
            assert!(String::from_utf8_lossy(message).contains("in the clear"));
            unsafe { virtx_bytes_free(error) };
        }

        #[test]
        fn a_request_s_arguments_read_with_their_defaults() {
            let mount: Mount = serde_json::from_str(r#"{"volume": "v"}"#).unwrap();
            assert_eq!((mount.volume.as_str(), mount.path.as_str()), ("v", ""));
            assert!(mount.source.is_none());

            let exec: Exec = serde_json::from_str(r#"{"sandbox": "s", "cmd": ["true"]}"#).unwrap();
            assert_eq!(
                (exec.sandbox.as_str(), exec.cmd.as_slice()),
                ("s", &["true".to_owned()][..])
            );
            assert!(!exec.stdin && exec.timeout_ms.is_none());

            let job: Job = serde_json::from_str(r#"{"sandbox": "s", "job": "j"}"#).unwrap();
            assert_eq!(job.job.as_deref(), Some("j"));
            assert!(!job.eof && job.signal.is_none() && job.data.is_none() && job.after.is_none());

            let log: ImageJobLogArgs = serde_json::from_str(r#"{"id": "j", "after": 3}"#).unwrap();
            assert_eq!((log.id.as_str(), log.after), ("j", Some(3)));
        }

        #[test]
        fn a_request_s_arguments_with_a_member_misspelled_or_missing_are_refused() {
            assert!(serde_json::from_str::<Name>(r#"{"nmae": "v"}"#).is_err());
            assert!(serde_json::from_str::<Name>(r#"{"name": "v", "extra": 1}"#).is_err());
            assert!(serde_json::from_str::<Id>("{}").is_err());
            assert!(serde_json::from_str::<Exec>(r#"{"sandbox": "s"}"#).is_err());
            assert!(
                serde_json::from_str::<Exec>(r#"{"sandbox": "s", "cmd": [], "timeout": 1}"#)
                    .is_err()
            );
            assert!(serde_json::from_str::<Job>(r#"{"sandbox": "s", "signal": 9}"#).is_err());
        }
    }
}
