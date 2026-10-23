//! The typed client: each method of [`method`](crate::method) as a call, over a
//! [`TransportClient`].

use std::io;

use futures_util::Stream;

use tokio::process::Command;

use crate::transport::Error;

use crate::{
    image::{ImageName, Recipe},
    method::{
        ImageBuild, ImageBuildArgs, ImageBuildOutput, ImageList, ImageListArgs,
        ImageListOutputItem, ImageRemove, ImageRemoveArgs, ImageRemoveOutput, Request,
        SnapshotCreate, SnapshotCreateArgs, SnapshotCreateOutput, SnapshotList, SnapshotListArgs,
        SnapshotListOutputItem, SnapshotRemove, SnapshotRemoveArgs, SnapshotRemoveOutput,
    },
    transport::{
        Event, TransportClient,
        r#impl::{stdio, streamable_http},
    },
};

/// A client of one server, over stdio or Streamable HTTP.
pub struct Client {
    inner: TransportClient,
}

impl Client {
    /// Start `command` as the server, and talk to it over its stdin and stdout. Must be called
    /// inside a tokio runtime.
    pub fn spawn(command: impl Into<Command>) -> io::Result<Client> {
        let (outgoing, incoming) = stdio::spawn(command)?;
        let inner = TransportClient::new(outgoing, incoming);
        Ok(Client { inner })
    }

    /// Reach the server at `url`, an `http` or `https` URL. Must be called inside a tokio
    /// runtime.
    pub fn connect(url: &str) -> anyhow::Result<Client> {
        let (outgoing, incoming) = streamable_http::connect(url)?;
        let inner = TransportClient::new(outgoing, incoming);
        Ok(Client { inner })
    }

    /// Every reference the server holds, and the image each points at.
    pub async fn list_image(&self) -> Result<Vec<ImageListOutputItem>, Error> {
        self.inner.request(ImageList::NAME, ImageListArgs {}).await
    }

    /// Build `recipe`, and point `reference` at it; without one the server picks it. The
    /// stream is the build's log, a [`Event::Progress`] a line, then what it built as its last
    /// item, an [`Event::Output`] or the error it failed with. Dropping it drops the build.
    pub fn build_image(
        &self,
        recipe: Recipe,
        reference: Option<impl Into<String>>,
    ) -> impl Stream<Item = Result<Event<ImageBuildOutput>, Error>> + Unpin + Send + '_ {
        Box::pin(self.inner.request_with_progress(
            ImageBuild::NAME,
            ImageBuildArgs {
                recipe,
                reference: reference.map(Into::into),
            },
        ))
    }

    /// Forget `image`, given as an [`ImageName`] or a string read by [`ImageName::parse`].
    pub async fn remove_image(&self, image: impl Into<ImageName>) -> Result<(), Error> {
        let _: ImageRemoveOutput = self
            .inner
            .request(
                ImageRemove::NAME,
                ImageRemoveArgs {
                    image: image.into(),
                },
            )
            .await?;
        Ok(())
    }

    /// Every snapshot the server holds.
    pub async fn list_snapshot(&self) -> Result<Vec<SnapshotListOutputItem>, Error> {
        self.inner
            .request(SnapshotList::NAME, SnapshotListArgs {})
            .await
    }

    /// Keep a new, empty upper layer under `name`; without one the server picks it.
    pub async fn create_snapshot(
        &self,
        name: Option<impl Into<String>>,
    ) -> Result<SnapshotCreateOutput, Error> {
        self.inner
            .request(
                SnapshotCreate::NAME,
                SnapshotCreateArgs {
                    name: name.map(Into::into),
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

    /// End the session; see [`TransportClient::close`].
    pub async fn close(self) {
        self.inner.close().await;
    }
}

#[cfg(test)]
mod tests {
    use futures_util::StreamExt;

    use super::*;
    use crate::transport::testing::connect;

    #[tokio::test]
    async fn a_build_streams_its_log_then_its_output() {
        let client = Client { inner: connect() };
        let mut build = client.build_image(Recipe::new("alpine:3.20"), Some("jq:latest"));
        let mut events = Vec::new();
        while let Some(event) = build.next().await {
            events.push(event.unwrap());
        }
        let message = "pulling alpine:3.20".to_string();
        let output = ImageBuildOutput {
            reference: "jq:latest".into(),
            digest: "sha256:0".into(),
        };
        assert_eq!(events, [Event::Progress(message), Event::Output(output)]);
    }

    #[tokio::test]
    async fn a_failed_build_ends_its_stream_with_the_error() {
        let client = Client { inner: connect() };
        let mut build = client.build_image(Recipe::new("missing"), None::<String>);
        assert!(matches!(build.next().await, Some(Ok(Event::Progress(_)))));
        assert!(matches!(build.next().await, Some(Err(Error::Call(_)))));
        assert!(build.next().await.is_none());
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
        ffi::{Runtime, VirtxBytes, VirtxFuture, VirtxStream, cbor, release, retain, share},
        method::{ImageBuildArgs, ImageRemoveArgs, SnapshotCreateArgs, SnapshotRemoveArgs},
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

    /// Reach the server at `url`, an `http` or `https` URL, and write the client, held once, to
    /// `client`.
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

    /// `images.list`: answers with the references, as
    /// `[{"reference": "name:tag", "digest": "algorithm:hex", "size": n}, ...]`.
    ///
    /// Every value here is CBOR, written in diagnostic notation. A request that takes arguments
    /// takes them as `args`, the `args_len` bytes of one map, the method's params.
    ///
    /// # Safety
    ///
    /// `client` is one `virtx_client_spawn` or `virtx_client_connect` wrote, still held.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_list_image(client: *mut VirtxClient) -> *mut VirtxFuture {
        unsafe { &(*client).0 }
            .future(|client| async move { client.list_image().await.map_err(|e| e.to_string()) })
    }

    /// `images.build`: builds `recipe` and points `reference` (`name:tag`) at it, or a
    /// reference the server picks if there is none, given
    /// `{"recipe": {"v": 1, "base": "alpine:3.20", "steps": [{"run": "..."}]}, "reference": "name:tag"}`.
    /// Its items are its log, a line each, as `{"progress": "..."}`, then what it built, as
    /// `{"output": {"reference": "name:tag", "digest": "algorithm:hex"}}`.
    ///
    /// # Safety
    ///
    /// `client` is as in `virtx_client_list_image`. `args` is `args_len` readable bytes, not
    /// read after this returns.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_build_image(
        client: *mut VirtxClient,
        args: *const u8,
        args_len: usize,
    ) -> *mut VirtxStream {
        let args = unsafe { cbor::<ImageBuildArgs>(args, args_len) };
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

    /// `images.remove`: forgets `image`, given
    /// `{"image": {"type": "reference", "reference": "name:tag"}}` or
    /// `{"image": {"type": "digest", "digest": "algorithm:hex"}}`. Answers with `null`.
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
        let args = unsafe { cbor::<ImageRemoveArgs>(args, args_len) };
        unsafe { &(*client).0 }.future(|client| async move {
            client
                .remove_image(args?.image)
                .await
                .map_err(|e| e.to_string())
        })
    }

    /// `snapshots.list`: answers with the snapshots, as
    /// `[{"name": "...", "size": n}, ...]`.
    ///
    /// # Safety
    ///
    /// `client` is as in `virtx_client_list_image`.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_list_snapshot(
        client: *mut VirtxClient,
    ) -> *mut VirtxFuture {
        unsafe { &(*client).0 }
            .future(|client| async move { client.list_snapshot().await.map_err(|e| e.to_string()) })
    }

    /// `snapshots.create`: keeps a new, empty upper layer under `name`, or a name the server
    /// picks if there is none, given `{"name": "..."}` or `{}`. Answers with `{"name": "..."}`.
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
        let args = unsafe { cbor::<SnapshotCreateArgs>(args, args_len) };
        unsafe { &(*client).0 }.future(|client| async move {
            client
                .create_snapshot(args?.name)
                .await
                .map_err(|e| e.to_string())
        })
    }

    /// `snapshots.remove`: deletes the snapshot `name` and its upper layer, given
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
        let args = unsafe { cbor::<SnapshotRemoveArgs>(args, args_len) };
        unsafe { &(*client).0 }.future(|client| async move {
            client
                .remove_snapshot(args?.name)
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
