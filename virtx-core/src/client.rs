//! The typed client: each method of [`method`](crate::method) as a call, over a
//! [`TransportClient`].

use std::io;

use crate::transport::Error;
use tokio::process::Command;

use crate::{
    image::{ImageName, Recipe},
    method::{
        ImageBuild, ImageBuildArgs, ImageBuildOutput, ImageList, ImageListArgs,
        ImageListOutputItem, ImageRemove, ImageRemoveArgs, ImageRemoveOutput, Request,
    },
    transport::{Transport, TransportClient},
};

/// A client of one server, over stdio or Streamable HTTP.
pub struct Client {
    inner: TransportClient,
}

impl Client {
    /// Start `command` as the server, and talk to it over its stdin and stdout. Must be called
    /// inside a tokio runtime.
    pub fn spawn(command: impl Into<Command>) -> io::Result<Client> {
        let (sender, receiver, transport) = Transport::spawn(command)?;
        let inner = TransportClient::new(sender, receiver, transport);
        Ok(Client { inner })
    }

    /// Reach the server at `url`, an `http` or `https` URL. Must be called inside a tokio
    /// runtime.
    pub fn connect(url: &str) -> anyhow::Result<Client> {
        let (sender, receiver, transport) = Transport::connect(url)?;
        let inner = TransportClient::new(sender, receiver, transport);
        Ok(Client { inner })
    }

    /// Every reference the server holds, and the image each points at.
    pub async fn list_image(&self) -> Result<Vec<ImageListOutputItem>, Error> {
        self.inner.request(ImageList::NAME, ImageListArgs {}).await
    }

    /// Build `recipe`, and point `reference` at it; without one the server picks it.
    pub async fn build_image(
        &self,
        recipe: Recipe,
        reference: Option<&str>,
    ) -> Result<ImageBuildOutput, Error> {
        self.inner
            .request(
                ImageBuild::NAME,
                ImageBuildArgs {
                    recipe,
                    reference: reference.map(str::to_string),
                },
            )
            .await
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

    /// End the session; see [`TransportClient::close`].
    pub async fn close(self) {
        self.inner.close().await;
    }
}

mod ffi {
    //! [`Client`] behind the C ABI: opening one, sharing it by reference count, and its methods,
    //! each of which starts the call and hands C the [`VirtxCall`].

    use std::ffi::{c_char, c_int};

    use tokio::process::Command;

    use super::Client;
    use crate::{
        ffi::{Runtime, VirtxCall, json, release, retain, share, text},
        image::ImageName,
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
        error: *mut *mut c_char,
    ) -> c_int {
        let opened = (|| {
            let mut command = Command::new(unsafe { text(program) }?);
            for i in 0..args_len {
                command.arg(unsafe { text(*args.add(i)) }?);
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
        error: *mut *mut c_char,
    ) -> c_int {
        let opened = unsafe { text(url) }
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

    /// Let go of `client`. The last holder's release answers every call not yet answered with
    /// `VIRTX_ERROR`, ends the session, and frees it; its calls stay to be polled and freed. Null
    /// does nothing.
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

    /// `image.list`: answers with the references, as
    /// `[{"reference": "name:tag", "digest": "algorithm:hex", "size": n}, ...]`.
    ///
    /// # Safety
    ///
    /// `client` is one `virtx_client_spawn` or `virtx_client_connect` wrote, still held.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_list_image(client: *mut VirtxClient) -> *mut VirtxCall {
        unsafe { &(*client).0 }
            .call(|client| async move { client.list_image().await.map_err(|e| e.to_string()) })
    }

    /// `image.build`: builds `recipe`, as
    /// `{"v": 1, "base": "alpine:3.20", "steps": [{"run": "..."}]}`, and points `reference`
    /// (`name:tag`) at it, or a reference the server picks if `reference` is null. Answers with
    /// `{"reference": "name:tag", "digest": "algorithm:hex"}`.
    ///
    /// # Safety
    ///
    /// `client` is as in `virtx_client_list_image`. `recipe` is NUL-terminated UTF-8, and
    /// `reference` is null or one too; neither is read after this returns.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_build_image(
        client: *mut VirtxClient,
        recipe: *const c_char,
        reference: *const c_char,
    ) -> *mut VirtxCall {
        let args = (|| {
            let recipe = unsafe { json(recipe) }?;
            let reference = match reference.is_null() {
                true => None,
                false => Some(unsafe { text(reference) }?.to_owned()),
            };
            Ok::<_, String>((recipe, reference))
        })();
        unsafe { &(*client).0 }.call(|client| async move {
            let (recipe, reference) = args?;
            client
                .build_image(recipe, reference.as_deref())
                .await
                .map_err(|e| e.to_string())
        })
    }

    /// `image.remove`: forgets `image`, as `{"type": "reference", "reference": "name:tag"}` or
    /// `{"type": "digest", "digest": "algorithm:hex"}`. Answers with `null`.
    ///
    /// # Safety
    ///
    /// `client` is as in `virtx_client_list_image`. `image` is NUL-terminated UTF-8, not read
    /// after this returns.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_client_remove_image(
        client: *mut VirtxClient,
        image: *const c_char,
    ) -> *mut VirtxCall {
        let image = unsafe { json::<ImageName>(image) };
        unsafe { &(*client).0 }.call(|client| async move {
            client.remove_image(image?).await.map_err(|e| e.to_string())
        })
    }

    #[cfg(test)]
    mod tests {
        use std::ptr;

        use super::*;
        use crate::ffi::virtx_string_free;

        #[test]
        fn spawning_nothing_fails_with_a_message() {
            let mut client = ptr::null_mut();
            let mut error = ptr::null_mut();
            let program = c"/nonexistent/virtx-server";
            let status = unsafe {
                virtx_client_spawn(program.as_ptr(), ptr::null(), 0, &mut client, &mut error)
            };
            assert_eq!(status, crate::ffi::VIRTX_ERROR);
            assert!(client.is_null());
            assert!(!error.is_null());
            unsafe { virtx_string_free(error) };
        }
    }
}
