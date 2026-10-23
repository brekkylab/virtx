//! The typed client: each method of [`method`](crate::method) as a call, over a
//! [`TransportClient`].

use std::io;

use jsonrpsee::core::client::Error;
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
