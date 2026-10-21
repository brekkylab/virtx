//! A registry: what it is asked, what it answers, and pulling what it answers into a
//! [`Cache`].
//!
//! The asking knows nothing of where an image is kept or what a layer holds: a manifest, a
//! config and blobs, as they arrive. Pulling is where the wire meets the cache, with
//! [`filesystem`] carrying each layer from one to the other.

use std::fmt::Display;
use std::io::Cursor;

use oci_client::Client;
use oci_client::client::ClientConfig;
use oci_client::manifest::{ImageIndexEntry, OciDescriptor, OciImageManifest};
use oci_client::secrets::RegistryAuth;
use oci_spec::image::ImageConfiguration;
use tokio::io::AsyncWrite;

use crate::cache::{self, Cache};
use crate::filesystem::{self, Changeset, Digest, MediaType};
use crate::image::Image;

/// How an image is named, being a registry, a repository, and a tag or a digest.
///
/// Its `Display` is the canonical spelling, so that `alpine:3.21` and
/// `docker.io/library/alpine:3.21` print the same. That is what makes it, and not a
/// string, what the cache is keyed by.
pub use oci_spec::distribution::Reference;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("pulling {reference}: {source}")]
    Registry {
        reference: String,
        #[source]
        source: Box<oci_client::errors::OciDistributionError>,
    },

    /// What was handed in as a name did not parse as one. Only what was wrong with it,
    /// not what it was: the conversion takes the name by value, so by the time it has
    /// failed there is nothing left to quote.
    #[error("not an OCI reference: {0}")]
    Reference(String),

    /// What the registry served, taken together, is not an image: a manifest and a config
    /// that disagree, or a layer that is not the one its config signs.
    #[error("{0} said something an image cannot say: {1}")]
    Manifest(String, String),

    #[error(transparent)]
    Cache(#[from] cache::Error),

    #[error(transparent)]
    Filesystem(#[from] filesystem::Error),

    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },
}

impl Error {
    fn io(context: impl Into<String>, source: std::io::Error) -> Error {
        Error::Io {
            context: context.into(),
            source,
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// A handle to a remote OCI registry to pull images from.
///
/// A few constraints apply:
/// - Always resolves a **linux** image, whatever the host platform is. Only the
///   architecture follows the host, because the guest that runs the image is Linux even
///   on a Mac or on Windows.
/// - Downloads only.
pub struct Registry {
    client: Client,
    auth: RegistryAuth,
}

impl Registry {
    /// A registry client that reads what anybody can read.
    pub fn new() -> Self {
        let config = ClientConfig {
            // Picks what the guest can run out of a multi-platform image. The OS is
            // linux whatever the host is, and only the architecture follows it, spelled
            // the way OCI spells it. One libkrun does not run on passes through under its
            // own name, which keeps the error honest.
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

    /// Put `reference` and everything behind it into `cache`, so that a boot can proceed
    /// offline. Only what is missing is downloaded, and a failed attempt keeps whatever did
    /// arrive.
    ///
    /// `reference` is anything a [`Reference`] can be read out of -- `"alpine:3.21"`, a
    /// `String`, or one already parsed.
    ///
    /// Each layer goes archive to tree to EROFS (see [`filesystem`]),
    /// one at a time: its compressed bytes and then its tree are held in memory only for
    /// as long as that takes, and one already in the cache is not asked for at all.
    ///
    /// ```no_run
    /// # async fn demo() -> Result<(), Box<dyn std::error::Error>> {
    /// use virtx_uvm::cache::Cache;
    /// use virtx_uvm::registry::Registry;
    ///
    /// Registry::default().pull("alpine:3.21", &Cache::default()).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn pull<E: Display>(
        &self,
        reference: impl TryInto<Reference, Error = E>,
        cache: &Cache,
    ) -> Result<Image> {
        // Before the registry is asked anything, so a name that is not a name costs no
        // round trip -- and so that what the cache files is the canonical spelling and not
        // whichever of its spellings this caller reached for.
        let reference = reference
            .try_into()
            .map_err(|e| Error::Reference(e.to_string()))?;

        let image = self.fetch(&reference, cache).await?;

        // After the image, so the reference never names one that is not here.
        cache.set_ref(&reference, &image.manifest)?;

        Ok(image)
    }

    /// What `reference` resolves to: the manifest, the digest that names it, and the
    /// config.
    ///
    /// The digest is the platform manifest's and not the index's, and only that one
    /// names what was pulled -- an index names every platform at once.
    pub async fn get_metadata(
        &self,
        reference: &Reference,
    ) -> Result<(OciImageManifest, Digest, ImageConfiguration)> {
        let (manifest, digest, config) = self
            .client
            .pull_manifest_and_config(reference, &self.auth)
            .await
            .map_err(|e| Error::Registry {
                reference: reference.to_string(),
                source: Box::new(e),
            })?;

        let config = ImageConfiguration::from_reader(config.as_bytes())
            .map_err(|e| Error::Manifest(reference.to_string(), format!("its config: {e}")))?;
        let digest = digest.parse::<Digest>().map_err(|e| {
            Error::Manifest(reference.to_string(), format!("its digest {digest}: {e}"))
        })?;

        Ok((manifest, digest, config))
    }

    /// Write one blob into `sink` as it arrives, verified against `descriptor` on the way
    /// past.
    ///
    /// Nothing accumulates anywhere, so a blob of any size costs whatever `sink` buffers
    /// and no more. A caller that does want the bytes in hand can pass a `Vec<u8>`, which
    /// is a sink like any other -- and then owns that decision where it can be seen.
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

    /// Put the image `reference` names and its layers into `cache`, without having
    /// `reference` name it there.
    ///
    /// What [`pull`](Registry::pull) does short of the name: for an image wanted on the way to another --
    /// the base a build starts from -- which nobody asked to keep under a name of its own.
    /// Its layers stay reachable through whatever is built on them.
    pub(crate) async fn fetch(&self, reference: &Reference, cache: &Cache) -> Result<Image> {
        let (manifest, digest, config) = self.get_metadata(reference).await?;

        // The manifest names each layer's compressed bytes and the config names its contents,
        // and nothing but their position pairs the two. A disagreement here is an image
        // nothing downstream can read correctly, so it is caught before anything is fetched.
        let diff_ids = config.rootfs().diff_ids();
        if diff_ids.len() != manifest.layers.len() {
            return Err(Error::Manifest(
                reference.to_string(),
                format!(
                    "{} layers in the manifest and {} diff_ids in the config",
                    manifest.layers.len(),
                    diff_ids.len()
                ),
            ));
        }
        let layers = diff_ids
            .iter()
            .map(|diff_id| {
                diff_id.parse::<Digest>().map_err(|e| {
                    Error::Manifest(reference.to_string(), format!("its diff_id {diff_id}: {e}"))
                })
            })
            .collect::<Result<Vec<Digest>>>()?;

        for (descriptor, expected) in manifest.layers.iter().zip(&layers) {
            // What the config signs this layer as, and so what the cache files it under.
            // Asked before anything is fetched, so a layer already converted costs neither the
            // download nor the conversion.
            if cache.has_layer(expected) {
                continue;
            }

            // The blob as the registry serves it, compressed. Held in memory and never
            // written to the cache: what is kept is the EROFS made of it. A capacity hint and
            // no more, so that a descriptor claiming a size nobody has cannot ask for it.
            let mut blob = Vec::with_capacity(descriptor.size.clamp(0, 1 << 30) as usize);
            self.pull_blob(reference, descriptor, &mut blob).await?;

            // Off the runtime, because uncompressing an archive and laying out a filesystem
            // is CPU that every other task on these threads would otherwise wait behind.
            let media_type = MediaType::from(descriptor.media_type.as_str());
            let cache = cache.clone();
            let expected = expected.clone();
            let image = reference.to_string();
            tokio::task::spawn_blocking(move || {
                let (tree, diff_id) = Changeset::new(&media_type, Cursor::new(blob))?.to_tree()?;

                // The config's word against the registry's bytes, asked before the tree is
                // written: a layer that uncompresses to something the config does not name is
                // not this image's layer.
                if diff_id != expected {
                    return Err(Error::Manifest(
                        image,
                        format!(
                            "a layer uncompresses to {diff_id}, and its config signs {expected}"
                        ),
                    ));
                }

                cache.put_layer(&expected, &tree)?;
                Ok(())
            })
            .await
            .map_err(|e| Error::io("converting a layer", std::io::Error::other(e)))??;
        }

        // Last, so nothing names an image whose layers did not all arrive. What is kept is
        // what a boot reads and nothing else: the registry is not asked again, and neither
        // the manifest nor the config is kept as the bytes it arrived as.
        let image = Image {
            manifest: digest,
            layers,
            platform: format!("{}/{}", config.os(), config.architecture()),
            config: config.config().clone(),
        };
        cache.put_image(&image)?;

        Ok(image)
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}
