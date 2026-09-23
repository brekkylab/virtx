use oci_client::client::ClientConfig;
use oci_client::manifest::{
    IMAGE_MANIFEST_MEDIA_TYPE, ImageIndexEntry, OCI_IMAGE_MEDIA_TYPE, OciDescriptor,
};
use oci_client::secrets::RegistryAuth;
use oci_client::{Client, Reference};
use tokio::io::AsyncWrite;

use crate::image::image::ImageConfig;

use super::{Digest, Error, ImageManifest, Result};

/// What a single-platform manifest may be served as.
const MANIFEST_MEDIA_TYPES: &[&str] = &[OCI_IMAGE_MEDIA_TYPE, IMAGE_MANIFEST_MEDIA_TYPE];

/// A handle to a remote OCI registry to pull images from.
///
/// A few constraints apply:
/// - Always resolves a **linux** image, whatever the host platform is. Only the
///   architecture follows the host, because the guest that runs the image is Linux even
///   on a Mac or on Windows.
/// - Downloads image only.
pub struct Registry {
    client: Client,
    auth: RegistryAuth,
}

impl Registry {
    /// A registry client that reads what anybody can read.
    pub fn new() -> Self {
        let config = ClientConfig {
            // Picks what the guest can run out of a multi-platform image. The OS is
            // linux whatever the host is, and only the architecture follows it,
            // spelled the way OCI spells it. One libkrun does not run on passes
            // through under its own name, which keeps the error honest.
            platform_resolver: Some(Box::new(|manifests: &[ImageIndexEntry]| {
                let wanted = match std::env::consts::ARCH {
                    "aarch64" => "arm64",
                    "x86_64" => "amd64",
                    other => other,
                };
                manifests
                    .iter()
                    .find(|entry| {
                        entry.platform.as_ref().is_some_and(|platform| {
                            platform.os.to_string() == "linux"
                                && platform.architecture.to_string() == wanted
                        })
                    })
                    .map(|entry| entry.digest.clone())
            })),
            ..ClientConfig::default()
        };
        Registry {
            client: Client::new(config),
            auth: RegistryAuth::Anonymous,
        }
    }

    /// A registry client with credentials, for a private repository.
    pub fn with_auth(mut self, auth: RegistryAuth) -> Self {
        self.auth = auth;
        self
    }

    /// What `reference` resolves to, as the registry put it: the manifest, the digest
    /// that names it, and the config.
    ///
    /// The config is bytes rather than an [`Image`](super::Image) because that is what arrived, and
    /// because a cache stores those bytes -- one that has been through a struct no
    /// longer hashes to what the manifest calls it. [`Image::read`](super::Image::read) is what turns the
    /// three into an image.
    ///
    /// The digest is the platform manifest's and not the index's, and only that one
    /// matches what the cache records.
    pub async fn get_metadata(
        &self,
        reference: &Reference,
    ) -> Result<(ImageManifest, Digest, ImageConfig)> {
        let (manifest, digest, config) = self
            .client
            .pull_manifest_and_config(reference, &self.auth)
            .await
            .map_err(|e| Error::Registry {
                reference: reference.to_string(),
                source: Box::new(e),
            })?;

        let config = ImageConfig::from_json_str(&config)
            .map_err(|e| Error::Manifest(reference.to_string(), format!("its config: {e}")))?;

        Ok((manifest, Digest::parse(digest)?, config))
    }

    /// The manifest of `reference`, byte for byte as the registry served it.
    ///
    /// Not [`get_metadata`](Self::get_metadata)'s [`ImageManifest`] re-serialised: a
    /// manifest digest is over exact bytes, and JSON that has been through a struct is
    /// no longer them. Anything that stores or hashes a manifest wants these.
    ///
    /// A reference with a tag on it may resolve to an index rather than to the platform
    /// manifest behind it. Pin it to a digest first if that matters.
    pub async fn get_manifest_raw(&self, reference: &Reference) -> Result<Vec<u8>> {
        let (raw, _) = self
            .client
            .pull_manifest_raw(reference, &self.auth, MANIFEST_MEDIA_TYPES)
            .await
            .map_err(|e| Error::Registry {
                reference: reference.to_string(),
                source: Box::new(e),
            })?;

        Ok(raw.to_vec())
    }

    /// Write one blob into `sink` as it arrives, verified against `descriptor` on the
    /// way past.
    ///
    /// Nothing accumulates anywhere, so a layer of any size costs whatever `sink`
    /// buffers and no more. A caller that does want the bytes in hand can pass a
    /// `Vec<u8>`, which is a sink like any other -- and then owns that decision where
    /// it can be seen.
    pub async fn pull_blob(
        &self,
        reference: &Reference,
        descriptor: &OciDescriptor,
        sink: impl AsyncWrite,
    ) -> Result<()> {
        self.client
            .pull_blob(reference, descriptor, sink)
            .await
            .map_err(|e| Error::Registry {
                reference: reference.to_string(),
                source: Box::new(e),
            })
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}
