//! The typed client: each method of [`method`](crate::method) as a call, over a
//! [`TransportClient`].

use std::io;

use futures_util::Stream;
use tokio::process::Command;

use crate::{
    image::{ImageBuild, ImageBuildArgs, ImageBuildOutput, Recipe},
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
    transport::{Error, TransportClient},
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
    inner: TransportClient,
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
    use crate::{method::Progress, transport::tests::connect};

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

mod ffi {
    //! [`Client`] behind the C ABI: opening one, sharing it by reference count, and its methods,
    //! each of which starts the request and hands C its [`VirtxFuture`] or [`VirtxStream`].

    use std::ffi::{CStr, c_char, c_int};

    use futures_util::StreamExt;
    use tokio::process::Command;

    use super::Client;
    use crate::{
        ffi::{Runtime, VirtxBytes, VirtxFuture, VirtxStream, json, release, retain, share},
        image::ImageBuildArgs,
        sandbox::{SandboxCloseArgs, SandboxExecArgs, SandboxInitArgs},
        snapshot::{SnapshotCreateArgs, SnapshotRemoveArgs},
        volume::{
            VolumeCreateArgs, VolumeMountCreateArgs, VolumeMountListArgs, VolumeMountRemoveArgs,
            VolumeRemoveArgs,
        },
    };

    /// A `Client`, as C holds it.
    pub struct VirtxClient(Runtime<Client>);

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
