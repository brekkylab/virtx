//! A remote registry, one OCI distribution call at a time.
//!
//! Nothing here knows where an image is kept once it has arrived, or what a pull or a push
//! is made of: each call is one of the spec's, said in `oci_spec`'s types, and putting
//! them together -- and filing what they answer -- is the caller's.

use oci_client::Client;
use oci_client::client::{ClientConfig, LayerDescriptor};
use oci_client::errors::OciDistributionError;
use oci_client::manifest::{IMAGE_MANIFEST_LIST_MEDIA_TYPE, IMAGE_MANIFEST_MEDIA_TYPE};
use oci_spec::image::{
    Arch, Descriptor, Digest, ImageConfiguration, ImageIndex, ImageManifest, MediaType, Os,
    Sha256Digest,
};
use sha2::{Digest as _, Sha256};
use tokio::io::AsyncWrite;

use crate::fstree::{self, FsTree};

/// How an image is named, being a registry, a repository, and a tag or a digest.
///
/// Its `Display` is the canonical spelling, so that `alpine:3.21` and
/// `docker.io/library/alpine:3.21` print the same.
pub use oci_spec::distribution::Reference;

/// Credentials for a remote registry.
#[derive(Clone, PartialEq, Eq)]
pub enum RegistryAuth {
    Basic { username: String, password: String },
    Bearer { token: String },
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The registry would not let these credentials, or no credentials, at `reference`.
    /// Apart from [`Registry`](Error::Registry) because it is the caller's to fix, not the
    /// registry's or the network's.
    #[error("{reference}: not authorized: {source}")]
    Unauthorized {
        reference: String,
        #[source]
        source: Box<OciDistributionError>,
    },

    #[error("{reference}: {source}")]
    Registry {
        reference: String,
        #[source]
        source: Box<OciDistributionError>,
    },

    /// What the registry served is not what it should be: a manifest that does not parse,
    /// or a config that disagrees with it.
    #[error("{0}: {1}")]
    Manifest(String, String),

    #[error(transparent)]
    Fstree(#[from] fstree::Error),

    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },
}

impl Error {
    /// `source`, as an error at `reference`: a refusal of the credentials told apart from
    /// everything else.
    fn registry(reference: &Reference, source: OciDistributionError) -> Error {
        let reference = reference.to_string();
        let source = Box::new(source);
        match *source {
            OciDistributionError::AuthenticationFailure(_)
            | OciDistributionError::UnauthorizedError { .. } => {
                Error::Unauthorized { reference, source }
            }
            _ => Error::Registry { reference, source },
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// A remote OCI registry, with one set of credentials.
///
/// The client under it keeps the credentials and the tokens they earn per registry host,
/// for as long as it lives. So a `Registry` is one caller's: two callers with different
/// credentials sharing one would each be acting with whichever logged in first.
///
/// A multi-platform image resolves to its **linux** manifest whatever the host is, and only
/// the architecture follows the host: the guest that runs the image is Linux even on a Mac.
pub struct Registry {
    client: Client,
    auth: oci_client::secrets::RegistryAuth,
}

impl std::fmt::Debug for Registry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Registry")
            .field("auth", &self.auth)
            .finish_non_exhaustive()
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new(None)
    }
}

impl Registry {
    /// A registry client with `auth`, or one that goes anonymously without it.
    pub fn new(auth: Option<RegistryAuth>) -> Self {
        let auth = match auth {
            None => oci_client::secrets::RegistryAuth::Anonymous,
            Some(RegistryAuth::Basic { username, password }) => {
                oci_client::secrets::RegistryAuth::Basic(username, password)
            }
            Some(RegistryAuth::Bearer { token }) => {
                oci_client::secrets::RegistryAuth::Bearer(token)
            }
        };
        Registry {
            client: Client::new(ClientConfig::default()),
            auth,
        }
    }

    /// `GET /v2/<name>/manifests/<reference>`: the image manifest `reference` names, and its
    /// digest.
    ///
    /// An index -- one manifest per platform -- is resolved to the linux manifest for the
    /// host's architecture, and fetched in turn. The digest is that manifest's, not the
    /// index's.
    pub async fn pull_manifest(&self, reference: &Reference) -> Result<(Digest, ImageManifest)> {
        let name = reference.to_string();
        let malformed = |what: String| Error::Manifest(name.clone(), what);
        // Docker's names for the same two, which `oci_spec` does not spell.
        let manifest_type = MediaType::ImageManifest.to_string();
        let index_type = MediaType::ImageIndex.to_string();

        let (body, digest) = self
            .client
            .pull_manifest_raw(
                reference,
                &self.auth,
                &[
                    &manifest_type,
                    IMAGE_MANIFEST_MEDIA_TYPE,
                    &index_type,
                    IMAGE_MANIFEST_LIST_MEDIA_TYPE,
                ],
            )
            .await
            .map_err(|e| Error::registry(reference, e))?;

        // An index is told by what it holds: its `mediaType` is optional.
        let value: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|e| malformed(format!("its manifest is not JSON: {e}")))?;
        let (body, digest) = if value.get("manifests").is_some() {
            let index: ImageIndex =
                serde_json::from_slice(&body).map_err(|e| malformed(format!("its index: {e}")))?;
            let arch = Arch::default();
            let entry = index
                .manifests()
                .iter()
                .find(|entry| {
                    entry.platform().as_ref().is_some_and(|platform| {
                        *platform.os() == Os::Linux && *platform.architecture() == arch
                    })
                })
                .ok_or_else(|| malformed(format!("no manifest for linux/{arch}")))?;
            let platform = reference.clone_with_digest(entry.digest().to_string());
            self.client
                .pull_manifest_raw(
                    &platform,
                    &self.auth,
                    &[&manifest_type, IMAGE_MANIFEST_MEDIA_TYPE],
                )
                .await
                .map_err(|e| Error::registry(reference, e))?
        } else {
            (body, digest)
        };

        let manifest: ImageManifest =
            serde_json::from_slice(&body).map_err(|e| malformed(format!("its manifest: {e}")))?;
        let digest = digest
            .parse()
            .map_err(|e| malformed(format!("its digest {digest}: {e}")))?;
        Ok((digest, manifest))
    }

    /// The config `manifest` names: `GET /v2/<name>/blobs/<digest>`, read as one.
    ///
    /// The manifest names each layer's compressed bytes and the config names its contents,
    /// and nothing but their position pairs the two. A disagreement in how many there are
    /// is an image nothing downstream can read correctly, so it is refused here.
    pub async fn pull_config(
        &self,
        reference: &Reference,
        manifest: &ImageManifest,
    ) -> Result<ImageConfiguration> {
        let descriptor = manifest.config();
        // A capacity hint and no more, so that a descriptor claiming a size nobody has
        // cannot ask for it.
        let mut blob = Vec::with_capacity(descriptor.size().min(1 << 20) as usize);
        self.pull_blob(reference, descriptor, &mut blob).await?;

        let name = reference.to_string();
        let config: ImageConfiguration = serde_json::from_slice(&blob)
            .map_err(|e| Error::Manifest(name.clone(), format!("its config: {e}")))?;
        let diff_ids = config.rootfs().diff_ids().len();
        if diff_ids != manifest.layers().len() {
            return Err(Error::Manifest(
                name,
                format!(
                    "{} layers in the manifest and {diff_ids} diff_ids in the config",
                    manifest.layers().len()
                ),
            ));
        }
        Ok(config)
    }

    /// `GET /v2/<name>/blobs/<digest>`: the blob `descriptor` names, written into `sink` as it
    /// arrives and checked against the descriptor's digest on the way past.
    ///
    /// Nothing accumulates anywhere, so a blob of any size costs whatever `sink` buffers.
    pub async fn pull_blob(
        &self,
        reference: &Reference,
        descriptor: &Descriptor,
        sink: impl AsyncWrite,
    ) -> Result<()> {
        // Only the manifest calls hand the client credentials; a blob asked for first would
        // go anonymously.
        self.client
            .store_auth_if_needed(reference.resolve_registry(), &self.auth)
            .await;
        let layer = LayerDescriptor {
            digest: descriptor.digest().as_ref(),
            urls: descriptor.urls(),
        };
        self.client
            .pull_blob(reference, &layer, sink)
            .await
            .map_err(|e| Error::registry(reference, e))
    }

    /// The layer `descriptor` names, fetched and read into a tree. Its bytes are checked
    /// against the descriptor's digest, as [`pull_blob`](Registry::pull_blob) checks every
    /// blob; what the image's config says of its contents is not asked.
    ///
    /// The compressed blob is held in memory for as long as the reading takes, and the
    /// reading is done off the runtime: uncompressing an archive is CPU that every other task
    /// on these threads would otherwise wait behind.
    pub async fn pull_layer(
        &self,
        reference: &Reference,
        descriptor: &Descriptor,
    ) -> Result<FsTree> {
        let mut blob = Vec::with_capacity(descriptor.size().min(1 << 30) as usize);
        self.pull_blob(reference, descriptor, &mut blob).await?;

        let media_type = descriptor.media_type().clone();
        let tree =
            tokio::task::spawn_blocking(move || FsTree::from_changeset(&media_type, &blob[..]))
                .await
                .map_err(|e| Error::Io {
                    context: "reading a layer".into(),
                    source: std::io::Error::other(e),
                })??;
        Ok(tree)
    }

    /// `HEAD /v2/<name>/blobs/<digest>`: whether the repository has the blob.
    pub async fn blob_exists(&self, reference: &Reference, digest: &Digest) -> Result<bool> {
        self.client
            .store_auth_if_needed(reference.resolve_registry(), &self.auth)
            .await;
        self.client
            .blob_exists(reference, digest.as_ref())
            .await
            .map_err(|e| Error::registry(reference, e))
    }

    /// `POST /v2/<name>/blobs/uploads/` and what follows: upload `blob`, which the registry
    /// checks against `digest`.
    pub async fn push_blob(
        &self,
        reference: &Reference,
        blob: Vec<u8>,
        digest: &Digest,
    ) -> Result<()> {
        self.client
            .store_auth_if_needed(reference.resolve_registry(), &self.auth)
            .await;
        self.client
            .push_blob(reference, blob, digest.as_ref())
            .await
            .map_err(|e| Error::registry(reference, e))?;
        Ok(())
    }

    /// `POST /v2/<name>/blobs/uploads/?mount=<digest>&from=<repository>`: give `reference`'s
    /// repository the blob `from`'s already has, on the same registry, without uploading it.
    pub async fn mount_blob(
        &self,
        reference: &Reference,
        from: &Reference,
        digest: &Digest,
    ) -> Result<()> {
        self.client
            .store_auth_if_needed(reference.resolve_registry(), &self.auth)
            .await;
        self.client
            .mount_blob(reference, from, digest.as_ref())
            .await
            .map_err(|e| Error::registry(reference, e))
    }

    /// `PUT /v2/<name>/manifests/<reference>`: put `manifest` under `reference`'s tag, and
    /// answer with its digest. Every blob it names has to be in the repository already.
    pub async fn push_manifest(
        &self,
        reference: &Reference,
        manifest: &ImageManifest,
    ) -> Result<Digest> {
        self.client
            .store_auth_if_needed(reference.resolve_registry(), &self.auth)
            .await;
        let body = serde_json::to_vec(manifest).map_err(|e| Error::Io {
            context: "serializing a manifest".into(),
            source: e.into(),
        })?;
        let digest = format!("{:x}", Sha256::digest(&body))
            .parse::<Sha256Digest>()
            .expect("sha2 writes 64 lowercase hex characters")
            .into();
        self.client
            .push_manifest_raw(
                reference,
                body,
                MediaType::ImageManifest.to_string().parse().unwrap(),
            )
            .await
            .map_err(|e| Error::registry(reference, e))?;
        Ok(digest)
    }

    /// `GET /v2/<name>/tags/list`: the tags in `reference`'s repository, as far as the
    /// registry answers in one page.
    pub async fn list_tags(&self, reference: &Reference) -> Result<Vec<String>> {
        self.client
            .list_tags(reference, &self.auth, None, None)
            .await
            .map(|response| response.tags)
            .map_err(|e| Error::registry(reference, e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_become_the_clients() {
        let registry = Registry::new(Some(RegistryAuth::Basic {
            username: "user".into(),
            password: "secret".into(),
        }));
        assert_eq!(
            registry.auth,
            oci_client::secrets::RegistryAuth::Basic("user".into(), "secret".into())
        );
        assert!(!format!("{registry:?}").contains("secret"));

        let registry = Registry::new(Some(RegistryAuth::Bearer {
            token: "token".into(),
        }));
        assert_eq!(
            registry.auth,
            oci_client::secrets::RegistryAuth::Bearer("token".into())
        );

        assert_eq!(
            Registry::default().auth,
            oci_client::secrets::RegistryAuth::Anonymous
        );
    }

    /// Needs a network: `cargo test -p virtx-uvm -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn pulls_a_public_image() {
        let registry = Registry::default();
        let reference: Reference = "alpine:3.21".parse().unwrap();
        let (_, manifest) = registry.pull_manifest(&reference).await.unwrap();
        let config = registry.pull_config(&reference, &manifest).await.unwrap();
        assert_eq!(config.os().to_string(), "linux");

        let tree = registry
            .pull_layer(&reference, &manifest.layers()[0])
            .await
            .unwrap();
        assert!(!tree.walk().is_empty());
        assert!(
            registry
                .blob_exists(&reference, manifest.layers()[0].digest())
                .await
                .unwrap()
        );
    }
}
