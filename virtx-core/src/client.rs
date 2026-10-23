//! The typed client: each method of [`method`](crate::method) as a call, over a
//! [`TransportClient`].

use std::io;

use jsonrpsee::core::{client::Error, traits::ToRpcParams};
use serde::Serialize;
use serde_json::value::RawValue;
use tokio::process::Command;

use crate::{
    image::{ImageName, Recipe},
    method::{
        ImageBuild, ImageBuildArgs, ImageBuildOutput, ImageList, ImageListArgs,
        ImageListOutputItem, ImageRemove, ImageRemoveArgs, Request,
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
        self.call::<ImageList>(ImageListArgs {}).await
    }

    /// Build `recipe`, and point `reference` at it; without one the server picks it.
    pub async fn build_image(
        &self,
        recipe: Recipe,
        reference: Option<&str>,
    ) -> Result<ImageBuildOutput, Error> {
        let args = ImageBuildArgs {
            recipe,
            reference: reference.map(str::to_string),
        };
        self.call::<ImageBuild>(args).await
    }

    /// Forget `image`.
    pub async fn remove_image(&self, image: ImageName) -> Result<(), Error> {
        self.call::<ImageRemove>(ImageRemoveArgs { image })
            .await
            .map(|_| ())
    }

    /// End the session; see [`TransportClient::close`].
    pub async fn close(self) {
        self.inner.close().await;
    }

    /// Call `R` with `args`, and wait for its output.
    async fn call<R: Request>(&self, args: R::Args) -> Result<R::Output, Error>
    where
        R::Args: Send,
    {
        self.inner.request(R::NAME, Args(args)).await
    }
}

/// A method's args as JSON-RPC `params`: the object they serialize to.
struct Args<T>(T);

impl<T: Serialize> ToRpcParams for Args<T> {
    fn to_rpc_params(self) -> Result<Option<Box<RawValue>>, serde_json::Error> {
        serde_json::value::to_raw_value(&self.0).map(Some)
    }
}
