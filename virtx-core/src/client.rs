//! The typed client: each method of [`method`](crate::method) as a call, and what is done to
//! images through the server's registry at `/images/v2/`, over a [`TransportClient`].

use std::{io, path::PathBuf};

use futures_util::Stream;
use tokio::process::Command;

use crate::{
    Error,
    image::{
        ImageBuild, ImageBuildArgs, ImageBuildOutput, ImageDownloadOutput, ImageListOutputItem,
        ImagePrune, ImagePruneArgs, ImagePruneOutput, ImagePull, ImagePullArgs, ImagePullOutput,
        ImagePush, ImagePushArgs, ImagePushOutput, ImageUploadOutput, Recipe, RegistryAuth, oci,
    },
    method::{Event, Request},
    sandbox::{
        SandboxClose, SandboxCloseArgs, SandboxCloseOutput, SandboxExec, SandboxExecArgs,
        SandboxExecOutput, SandboxInit, SandboxInitArgs, SandboxInitOutput, SandboxList,
        SandboxListArgs, SandboxListOutputItem,
    },
    snapshot::{
        SnapshotCreate, SnapshotCreateArgs, SnapshotCreateOutput, SnapshotList, SnapshotListArgs,
        SnapshotListOutputItem, SnapshotRemove, SnapshotRemoveArgs, SnapshotRemoveOutput,
    },
    transport::TransportClient,
    volume::{
        MountSource, VolumeCreate, VolumeCreateArgs, VolumeCreateOutput, VolumeList,
        VolumeListArgs, VolumeListOutputItem, VolumeMountCreate, VolumeMountCreateArgs,
        VolumeMountCreateOutput, VolumeMountList, VolumeMountListArgs, VolumeMountListOutputItem,
        VolumeMountRemove, VolumeMountRemoveArgs, VolumeMountRemoveOutput, VolumeRemove,
        VolumeRemoveArgs, VolumeRemoveOutput,
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

    /// Reach the server at `url`, its base `http` or `https` URL.
    pub fn connect(url: &str) -> anyhow::Result<Client> {
        let inner = TransportClient::connect(url)?;
        Ok(Client { inner })
    }

    /// Build `recipe`, and point `reference` at it. The stream is the build's log, a [`Event::Progress`] a line, then what it built as its last
    /// item, an [`Event::Output`] or the error it failed with. Dropping it drops the build.
    pub fn build_image(
        &self,
        recipe: Recipe,
        reference: impl Into<String>,
    ) -> impl Stream<Item = Result<Event<ImageBuildOutput, String>, Error>> + Unpin + Send + '_
    {
        Box::pin(self.inner.request_with_progress(
            ImageBuild::NAME,
            ImageBuildArgs {
                recipe,
                reference: reference.into(),
            },
        ))
    }

    /// Pull `reference` from its registry, and point it at what it pulls; without `auth` the
    /// server pulls anonymously. The stream is the pull's log, a [`Event::Progress`] a line,
    /// then what it pulled as its last item, an [`Event::Output`] or the error it failed with.
    /// Dropping it drops the pull.
    pub fn pull_image(
        &self,
        reference: impl Into<String>,
        auth: Option<RegistryAuth>,
    ) -> impl Stream<Item = Result<Event<ImagePullOutput, String>, Error>> + Unpin + Send + '_ {
        Box::pin(self.inner.request_with_progress(
            ImagePull::NAME,
            ImagePullArgs {
                reference: reference.into(),
                auth,
            },
        ))
    }

    /// Push the image `reference` points at to the registry `reference` names; without `auth`
    /// the server pushes anonymously. The stream is the push's log, a [`Event::Progress`] a
    /// line, then what it pushed as its last item, an [`Event::Output`] or the error it failed
    /// with. Dropping it drops the push.
    pub fn push_image(
        &self,
        reference: impl Into<String>,
        auth: Option<RegistryAuth>,
    ) -> impl Stream<Item = Result<Event<ImagePushOutput, String>, Error>> + Unpin + Send + '_ {
        Box::pin(self.inner.request_with_progress(
            ImagePush::NAME,
            ImagePushArgs {
                reference: reference.into(),
                auth,
            },
        ))
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
        self.inner
            .request(ImagePrune::NAME, ImagePruneArgs {})
            .await
    }

    /// Every snapshot the server holds.
    pub async fn list_snapshot(&self) -> Result<Vec<SnapshotListOutputItem>, Error> {
        self.inner
            .request(SnapshotList::NAME, SnapshotListArgs {})
            .await
    }

    /// Keep a new, empty upper layer under `name`, holding at most `disk_gib` GiB; without
    /// either the server picks it.
    pub async fn create_snapshot(
        &self,
        name: Option<impl Into<String>>,
        disk_gib: Option<u32>,
    ) -> Result<SnapshotCreateOutput, Error> {
        self.inner
            .request(
                SnapshotCreate::NAME,
                SnapshotCreateArgs {
                    name: name.map(Into::into),
                    disk_gib,
                },
            )
            .await
    }

    /// Delete the snapshot `name` and its upper layer.
    pub async fn remove_snapshot(&self, name: impl Into<String>) -> Result<(), Error> {
        let _: SnapshotRemoveOutput = self
            .inner
            .request(
                SnapshotRemove::NAME,
                SnapshotRemoveArgs { name: name.into() },
            )
            .await?;
        Ok(())
    }

    /// Every volume the server holds.
    pub async fn list_volume(&self) -> Result<Vec<VolumeListOutputItem>, Error> {
        self.inner
            .request(VolumeList::NAME, VolumeListArgs {})
            .await
    }

    /// Keep a new, empty volume under `name`; without one the server picks it.
    pub async fn create_volume(
        &self,
        name: Option<impl Into<String>>,
    ) -> Result<VolumeCreateOutput, Error> {
        self.inner
            .request(
                VolumeCreate::NAME,
                VolumeCreateArgs {
                    name: name.map(Into::into),
                },
            )
            .await
    }

    /// Delete the volume `name` and everything in it.
    pub async fn remove_volume(&self, name: impl Into<String>) -> Result<(), Error> {
        let _: VolumeRemoveOutput = self
            .inner
            .request(VolumeRemove::NAME, VolumeRemoveArgs { name: name.into() })
            .await?;
        Ok(())
    }

    /// The mounts in `volume`, without their credentials.
    pub async fn list_volume_mount(
        &self,
        volume: impl Into<String>,
    ) -> Result<Vec<VolumeMountListOutputItem>, Error> {
        self.inner
            .request(
                VolumeMountList::NAME,
                VolumeMountListArgs {
                    volume: volume.into(),
                },
            )
            .await
    }

    /// Mount `source` at `path` in `volume`, a directory the mount creates.
    pub async fn create_volume_mount(
        &self,
        volume: impl Into<String>,
        path: impl Into<String>,
        source: MountSource,
    ) -> Result<(), Error> {
        let _: VolumeMountCreateOutput = self
            .inner
            .request(
                VolumeMountCreate::NAME,
                VolumeMountCreateArgs {
                    volume: volume.into(),
                    path: path.into(),
                    source,
                },
            )
            .await?;
        Ok(())
    }

    /// Take away the mount at `path` in `volume`, leaving its source as it is.
    pub async fn remove_volume_mount(
        &self,
        volume: impl Into<String>,
        path: impl Into<String>,
    ) -> Result<(), Error> {
        let _: VolumeMountRemoveOutput = self
            .inner
            .request(
                VolumeMountRemove::NAME,
                VolumeMountRemoveArgs {
                    volume: volume.into(),
                    path: path.into(),
                },
            )
            .await?;
        Ok(())
    }

    /// Every sandbox the server holds.
    pub async fn list_sandbox(&self) -> Result<Vec<SandboxListOutputItem>, Error> {
        self.inner
            .request(SandboxList::NAME, SandboxListArgs {})
            .await
    }

    /// Make a sandbox as `args` says; see [`SandboxInitArgs::new`].
    pub async fn init_sandbox(&self, args: SandboxInitArgs) -> Result<SandboxInitOutput, Error> {
        self.inner.request(SandboxInit::NAME, args).await
    }

    /// Run `cmd` in `sandbox` to its end, killed after `timeout_ms` if one is given. The stream
    /// is its output, an [`Event::Progress`] a line as it is written, then how it ended as its
    /// last item, an [`Event::Output`] or the error it failed with. Dropping it drops the
    /// command.
    pub fn exec_sandbox(
        &self,
        sandbox: impl Into<String>,
        cmd: impl IntoIterator<Item = impl Into<String>>,
        timeout_ms: Option<u64>,
    ) -> impl Stream<Item = Result<Event<SandboxExecOutput, String>, Error>> + Unpin + Send + '_
    {
        Box::pin(self.inner.request_with_progress(
            SandboxExec::NAME,
            SandboxExecArgs {
                sandbox: sandbox.into(),
                cmd: cmd.into_iter().map(Into::into).collect(),
                timeout_ms,
            },
        ))
    }

    /// Stop `sandbox` and forget its id, keeping its snapshot and volumes.
    pub async fn close_sandbox(&self, sandbox: impl Into<String>) -> Result<(), Error> {
        let _: SandboxCloseOutput = self
            .inner
            .request(
                SandboxClose::NAME,
                SandboxCloseArgs {
                    sandbox: sandbox.into(),
                },
            )
            .await?;
        Ok(())
    }

    /// End the session; see [`TransportClient::close`].
    pub async fn close(self) {
        self.inner.close().await;
    }
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use futures_util::StreamExt;

    use super::*;
    use crate::{method::Progress, rpc::tests::connect};

    /// The first line of a log, as the test methods send it.
    fn line(line: &str) -> Progress<String> {
        Progress {
            progress: 1,
            total: None,
            content: Some(line.into()),
        }
    }

    #[tokio::test]
    async fn a_build_streams_its_log_then_its_output() {
        let client = Client { inner: connect() };
        let mut build = client.build_image(Recipe::new("alpine:3.20"), "jq:latest");
        let mut events = Vec::new();
        while let Some(event) = build.next().await {
            events.push(event.unwrap());
        }
        let output = ImageBuildOutput {
            digest: "sha256:0".into(),
        };
        assert_eq!(
            events,
            [
                Event::Progress(line("pulling alpine:3.20")),
                Event::Output(output)
            ]
        );
    }

    #[tokio::test]
    async fn a_failed_build_ends_its_stream_with_the_error() {
        let client = Client { inner: connect() };
        let mut build = client.build_image(Recipe::new("missing"), "missing:latest");
        assert!(matches!(build.next().await, Some(Ok(Event::Progress(_)))));
        assert!(matches!(build.next().await, Some(Err(Error::Call(_)))));
        assert!(build.next().await.is_none());
    }

    #[tokio::test]
    async fn a_pull_streams_its_log_then_its_output() {
        let client = Client { inner: connect() };
        let events: Vec<_> = client
            .pull_image("alpine:3.20", None)
            .map(Result::unwrap)
            .collect()
            .await;
        let output = ImagePullOutput {
            digest: "sha256:0".into(),
        };
        assert_eq!(
            events,
            [
                Event::Progress(line("pulling alpine:3.20")),
                Event::Output(output)
            ]
        );
    }

    #[tokio::test]
    async fn a_failed_pull_ends_its_stream_with_the_error() {
        let client = Client { inner: connect() };
        let mut pull = client.pull_image("missing", None);
        assert!(matches!(pull.next().await, Some(Ok(Event::Progress(_)))));
        assert!(matches!(pull.next().await, Some(Err(Error::Call(_)))));
        assert!(pull.next().await.is_none());
    }

    #[tokio::test]
    async fn a_prune_answers_what_it_deleted() {
        let client = Client { inner: connect() };
        let pruned = client.prune_image().await.unwrap();
        assert_eq!(pruned.images, ["sha256:0"]);
        assert_eq!(pruned.reclaimed, 1024);
    }

    #[tokio::test]
    async fn a_push_carries_its_credentials_and_streams_its_log_then_its_output() {
        let client = Client { inner: connect() };
        let auth = RegistryAuth::Bearer {
            token: "token".into(),
        };
        let events: Vec<_> = client
            .push_image("ghcr.io/owner/jq:latest", Some(auth))
            .map(Result::unwrap)
            .collect()
            .await;
        let output = ImagePushOutput {
            digest: "sha256:0".into(),
        };
        assert_eq!(
            events,
            [
                Event::Progress(line("pushing ghcr.io/owner/jq:latest as bearer")),
                Event::Output(output)
            ]
        );
    }

    #[tokio::test]
    async fn an_exec_streams_its_output_then_how_it_ended() {
        let client = Client { inner: connect() };
        let mut exec = client.exec_sandbox("s", ["echo", "hi"], None);
        let mut events = Vec::new();
        while let Some(event) = exec.next().await {
            events.push(event.unwrap());
        }
        let output = SandboxExecOutput { code: 0 };
        assert_eq!(events, [Event::Progress(line("hi")), Event::Output(output)]);
    }
}

#[cfg(feature = "ffi")]
pub(crate) mod ffi {
    //! [`Client`] behind the C ABI: opening one, sharing it by reference count, and its methods,
    //! each of which starts the request and hands C its [`VirtxFuture`] or [`VirtxStream`].

    use std::ffi::{CStr, c_char, c_int};

    use futures_util::StreamExt;
    use tokio::process::Command;

    use super::Client;
    use crate::{
        ffi::{Runtime, VirtxBytes, VirtxFuture, VirtxStream, json, release, retain, share},
        image::{
            ImageBuildArgs, ImageDownloadArgs, ImagePullArgs, ImagePushArgs, ImageRemoveArgs,
            ImageTagArgs, ImageUntagArgs, ImageUploadArgs,
        },
        sandbox::{SandboxCloseArgs, SandboxExecArgs, SandboxInitArgs},
        snapshot::{SnapshotCreateArgs, SnapshotRemoveArgs},
        volume::{
            VolumeCreateArgs, VolumeMountCreateArgs, VolumeMountListArgs, VolumeMountRemoveArgs,
            VolumeRemoveArgs,
        },
    };

    /// A `Client`, as C holds it.
    pub struct VirtxClient(pub(crate) Runtime<Client>);

    /// Start `program` with `args_len` arguments from `args` as the server, over its stdin and
    /// stdout, and write the client, held once, to `client`.
    ///
    /// # Safety
    ///
    /// `program` and each of the `args_len` pointers in `args` are NUL-terminated UTF-8. `client`
    /// is writable. `error` is null or writable.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_spawn(
        program: *const c_char,
        args: *const *const c_char,
        args_len: usize,
        client: *mut *mut VirtxClient,
        error: *mut VirtxBytes,
    ) -> c_int {
        let opened = (|| {
            let utf8 = |ptr| {
                unsafe { CStr::from_ptr(ptr) }
                    .to_str()
                    .map_err(|e| e.to_string())
            };
            let mut command = Command::new(utf8(program)?);
            for i in 0..args_len {
                command.arg(utf8(unsafe { *args.add(i) })?);
            }
            Runtime::open(|| {
                Client::spawn(command).map_err(|e| format!("starting the server: {e}"))
            })
        })();
        unsafe { share(opened.map(VirtxClient), client, error) }
    }

    /// Reach the server at `url`, its base `http` or `https` URL (`/rpc` is under it), and write
    /// the client, held once, to `client`.
    ///
    /// # Safety
    ///
    /// `url` is NUL-terminated UTF-8. `client` is writable. `error` is null or writable.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_connect(
        url: *const c_char,
        client: *mut *mut VirtxClient,
        error: *mut VirtxBytes,
    ) -> c_int {
        let opened = unsafe { CStr::from_ptr(url) }
            .to_str()
            .map_err(|e| e.to_string())
            .and_then(|url| Runtime::open(|| Client::connect(url).map_err(|e| format!("{e:#}"))));
        unsafe { share(opened.map(VirtxClient), client, error) }
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

    /// `image.build`: builds `recipe` and points `reference` (`name:tag`) at it, given
    /// `{"recipe": {"v": 1, "base": "alpine:3.20", "steps": [{"run": "..."}]}, "reference": "name:tag"}`.
    /// Its items are its log, a line each, as
    /// `{"progress": {"progress": n, "total": n, "content": "..."}}` (`total` and `content` each
    /// optional), then what it built, as `{"output": {"digest": "algorithm:hex"}}`.
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
    ) -> *mut VirtxStream {
        let args = unsafe { json::<ImageBuildArgs>(args, args_len) };
        unsafe { &(*client).0 }.stream(|client| {
            async_stream::stream! {
                let ImageBuildArgs { recipe, reference } = match args {
                    Ok(args) => args,
                    Err(error) => {
                        yield Err(error);
                        return;
                    }
                };
                let mut build = client.build_image(recipe, reference);
                while let Some(event) = build.next().await {
                    yield event.map_err(|e| e.to_string());
                }
            }
        })
    }

    /// `image.pull`: pulls `reference` from its registry and points it at what it pulls, given
    /// `{"reference": "alpine:3.20", "auth": {...}}`, the optional auth as
    /// `{"type": "basic", "username": "...", "password": "..."}` or
    /// `{"type": "bearer", "token": "..."}`. Its items are its log, as
    /// `virtx_client_build_image`'s are, then what it pulled, as
    /// `{"output": {"digest": "algorithm:hex"}}`.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_pull_image(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxStream {
        let args = unsafe { json::<ImagePullArgs>(args, args_len) };
        unsafe { &(*client).0 }.stream(|client| {
            async_stream::stream! {
                let ImagePullArgs { reference, auth } = match args {
                    Ok(args) => args,
                    Err(error) => {
                        yield Err(error);
                        return;
                    }
                };
                let mut pull = client.pull_image(reference, auth);
                while let Some(event) = pull.next().await {
                    yield event.map_err(|e| e.to_string());
                }
            }
        })
    }

    /// `image.push`: pushes the image `reference` points at to the registry `reference` names,
    /// given `{"reference": "ghcr.io/owner/name:tag", "auth": {...}}`, the optional auth as in
    /// `virtx_client_pull_image`. Its items are its log, as `virtx_client_build_image`'s are,
    /// then what it pushed, as `{"output": {"digest": "algorithm:hex"}}`.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_push_image(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxStream {
        let args = unsafe { json::<ImagePushArgs>(args, args_len) };
        unsafe { &(*client).0 }.stream(|client| {
            async_stream::stream! {
                let ImagePushArgs { reference, auth } = match args {
                    Ok(args) => args,
                    Err(error) => {
                        yield Err(error);
                        return;
                    }
                };
                let mut push = client.push_image(reference, auth);
                while let Some(event) = push.next().await {
                    yield event.map_err(|e| e.to_string());
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
        unsafe { &(*client).0 }
            .future(|client| async move { client.list_image().await.map_err(|e| e.to_string()) })
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
            client
                .tag_image(source, target)
                .await
                .map_err(|e| e.to_string())
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
        unsafe { &(*client).0 }.future(|client| async move {
            client
                .untag_image(args?.reference)
                .await
                .map_err(|e| e.to_string())
        })
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
        unsafe { &(*client).0 }.future(|client| async move {
            client
                .remove_image(args?.reference)
                .await
                .map_err(|e| e.to_string())
        })
    }

    /// Writes the image `reference` names to `path` as an OCI image layout in a tar archive,
    /// given `{"reference": "name:tag" or "name@algorithm:hex", "path": "..."}`. Its items are
    /// its log, a line a blob, as `virtx_client_build_image`'s are, then what it wrote, as
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
                    yield event.map_err(|e| e.to_string());
                }
            }
        })
    }

    /// Uploads the image in the OCI image layout archive at `path` and points the tag
    /// `reference` at it, given `{"path": "...", "reference": "name:tag"}`. Its items are its
    /// log, a line a blob, as `virtx_client_build_image`'s are, then what it uploaded, as
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
                    yield event.map_err(|e| e.to_string());
                }
            }
        })
    }

    /// `image.prune`: deletes each image no tag points at and no sandbox uses, and what only
    /// such images held. Answers with `{"images": ["algorithm:hex", ...], "reclaimed": n}`, the
    /// digests deleted and the bytes freed.
    ///
    /// # Safety
    ///
    /// `client` is as in `virtx_client_list_snapshot`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_prune_image(
        client: *mut VirtxClient,
    ) -> *mut VirtxFuture {
        unsafe { &(*client).0 }
            .future(|client| async move { client.prune_image().await.map_err(|e| e.to_string()) })
    }

    /// `snapshot.list`: answers with the snapshots, as
    /// `[{"name": "...", "size": n, "in_use": false}, ...]`.
    ///
    /// Every value here is JSON. A request that takes arguments takes them as `args`, the
    /// `args_len` bytes of one object, the method's params.
    ///
    /// # Safety
    ///
    /// `client` is one `virtx_client_spawn` or `virtx_client_connect` wrote, still held.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_list_snapshot(
        client: *mut VirtxClient,
    ) -> *mut VirtxFuture {
        unsafe { &(*client).0 }
            .future(|client| async move { client.list_snapshot().await.map_err(|e| e.to_string()) })
    }

    /// `snapshot.create`: keeps a new, empty upper layer under `name`, holding at most
    /// `disk_gib` GiB, the server picking either one not given, given
    /// `{"name": "...", "disk_gib": 8}` or less. Answers with `{"name": "..."}`.
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
            client
                .create_snapshot(args.name, args.disk_gib)
                .await
                .map_err(|e| e.to_string())
        })
    }

    /// `snapshot.remove`: deletes the snapshot `name` and its upper layer, given
    /// `{"name": "..."}`. Answers with `null`.
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
        let args = unsafe { json::<SnapshotRemoveArgs>(args, args_len) };
        unsafe { &(*client).0 }.future(|client| async move {
            client
                .remove_snapshot(args?.name)
                .await
                .map_err(|e| e.to_string())
        })
    }

    /// `volume.list`: answers with the volumes, as `[{"name": "...", "size": n}, ...]`.
    ///
    /// # Safety
    ///
    /// `client` is as in `virtx_client_list_snapshot`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_list_volume(
        client: *mut VirtxClient,
    ) -> *mut VirtxFuture {
        unsafe { &(*client).0 }
            .future(|client| async move { client.list_volume().await.map_err(|e| e.to_string()) })
    }

    /// `volume.create`: keeps a new, empty volume under `name`, or a name the server picks if
    /// there is none, given `{"name": "..."}` or `{}`. Answers with `{"name": "..."}`.
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
        unsafe { &(*client).0 }.future(|client| async move {
            client
                .create_volume(args?.name)
                .await
                .map_err(|e| e.to_string())
        })
    }

    /// `volume.remove`: deletes the volume `name` and everything in it, given
    /// `{"name": "..."}`. Answers with `null`.
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
        let args = unsafe { json::<VolumeRemoveArgs>(args, args_len) };
        unsafe { &(*client).0 }.future(|client| async move {
            client
                .remove_volume(args?.name)
                .await
                .map_err(|e| e.to_string())
        })
    }

    /// `volume.mount.list`: given `{"volume": "..."}`, answers with the volume's mounts, as
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
        let args = unsafe { json::<VolumeMountListArgs>(args, args_len) };
        unsafe { &(*client).0 }.future(|client| async move {
            client
                .list_volume_mount(args?.volume)
                .await
                .map_err(|e| e.to_string())
        })
    }

    /// `volume.mount.create`: given `{"volume": "...", "path": "...", "source": {...}}`, the
    /// source as `{"type": "notion", "api_key": "..."}`,
    /// `{"type": "gdrive", "client_id": "...", "client_secret": "...", "refresh_token": "..."}`
    /// or `{"type": "onedrive", "client_id": "...", "refresh_token": "..."}` (and
    /// `"client_secret"` if it has one), mounts it. Answers with `null`.
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
        let args = unsafe { json::<VolumeMountCreateArgs>(args, args_len) };
        unsafe { &(*client).0 }.future(|client| async move {
            let VolumeMountCreateArgs {
                volume,
                path,
                source,
            } = args?;
            client
                .create_volume_mount(volume, path, source)
                .await
                .map_err(|e| e.to_string())
        })
    }

    /// `volume.mount.remove`: given `{"volume": "...", "path": "..."}`, takes the mount away.
    /// Answers with `null`.
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
        let args = unsafe { json::<VolumeMountRemoveArgs>(args, args_len) };
        unsafe { &(*client).0 }.future(|client| async move {
            let VolumeMountRemoveArgs { volume, path } = args?;
            client
                .remove_volume_mount(volume, path)
                .await
                .map_err(|e| e.to_string())
        })
    }

    /// `sandbox.list`: answers with the sandboxes, as
    /// `[{"id": "...", "image": "algorithm:hex", "snapshot": "...", "running": bool,
    /// "ports": [{"port": n, "url": "..."}]}, ...]`, `snapshot` only for one that has one and
    /// `ports` only for one that opened some.
    ///
    /// # Safety
    ///
    /// `client` is as in `virtx_client_list_snapshot`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_list_sandbox(
        client: *mut VirtxClient,
    ) -> *mut VirtxFuture {
        unsafe { &(*client).0 }
            .future(|client| async move { client.list_sandbox().await.map_err(|e| e.to_string()) })
    }

    /// `sandbox.init`: makes a sandbox, given
    /// `{"image": {...}, "snapshot": "...", "network": bool, "ports": [n],
    /// "vcpus": n, "memory_mib": n, "gpu": bool, "gpu_memory_mib": n,
    /// "volumes": [{"volume": "...", "path": "/...", "readonly": bool}]}`, the image as
    /// `{"type": "tag", "tag": "name:tag"}` or
    /// `{"type": "digest", "digest": "algorithm:hex"}` and all but it optional. Answers with
    /// `{"id": "...", "ports": [{"port": n, "url": "..."}]}`, `ports` only if it was given some.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_init_sandbox(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxFuture {
        let args = unsafe { json::<SandboxInitArgs>(args, args_len) };
        unsafe { &(*client).0 }.future(|client| async move {
            client.init_sandbox(args?).await.map_err(|e| e.to_string())
        })
    }

    /// `sandbox.exec`: given `{"sandbox": "...", "cmd": ["..."], "timeout_ms": n}`,
    /// `timeout_ms` optional, runs `cmd` to its end. Its items are its output, a line each as
    /// it is written, as `{"progress": ...}` as `virtx_client_build_image`'s are, then how it
    /// ended, as `{"output": {"code": n}}`.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_exec_sandbox(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxStream {
        let args = unsafe { json::<SandboxExecArgs>(args, args_len) };
        unsafe { &(*client).0 }.stream(|client| {
            async_stream::stream! {
                let SandboxExecArgs { sandbox, cmd, timeout_ms } = match args {
                    Ok(args) => args,
                    Err(error) => {
                        yield Err(error);
                        return;
                    }
                };
                let mut exec = client.exec_sandbox(sandbox, cmd, timeout_ms);
                while let Some(event) = exec.next().await {
                    yield event.map_err(|e| e.to_string());
                }
            }
        })
    }

    /// `sandbox.close`: given `{"sandbox": "..."}`, stops the sandbox. Answers with `null`.
    ///
    /// # Safety
    ///
    /// As `virtx_client_build_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_close_sandbox(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxFuture {
        let args = unsafe { json::<SandboxCloseArgs>(args, args_len) };
        unsafe { &(*client).0 }.future(|client| async move {
            client
                .close_sandbox(args?.sandbox)
                .await
                .map_err(|e| e.to_string())
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
    }
}
