//! A remote registry, as a client of it: each of the OCI distribution spec's calls, said in
//! `oci_spec`'s types, and a pull and a push of a whole image between it and a [`Store`], as
//! `docker pull` and `docker push` do.
//!
//! A registry names an image and its layers by what it was sent, and the store by what they
//! hold, so the two names are not the same. An origin says which is which: a pull records what
//! each blob and manifest became, and a push what it sent each layer and image as, so the next
//! copy either way skips what the other side has already.

use std::io::Write;

use flate2::Compression;
use flate2::write::GzEncoder;
use oci_client::Client;
use oci_client::client::{ClientConfig, LayerDescriptor};
use oci_client::errors::OciDistributionError;
use oci_client::manifest::{IMAGE_MANIFEST_LIST_MEDIA_TYPE, IMAGE_MANIFEST_MEDIA_TYPE};
use oci_spec::image::{
    Arch, Descriptor, Digest, ImageConfiguration, ImageIndex, ImageManifest, ImageManifestBuilder,
    MediaType, Os, Sha256Digest,
};
use sha2::{Digest as _, Sha256};
use tokio::io::AsyncWrite;
use virtx_core::RegistryAuth;

use super::store::{self, Store};
use crate::fstree::{self, FsTree};

/// How an image is named, being a registry, a repository, and a tag or a digest.
///
/// Its `Display` is the canonical spelling, so that `alpine:3.21` and
/// `docker.io/library/alpine:3.21` print the same.
pub use oci_spec::distribution::Reference;

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

    /// A push of a reference that tags nothing in the store.
    #[error("no image here is tagged {0}")]
    Untagged(String),

    #[error(transparent)]
    Store(#[from] store::Error),

    #[error(transparent)]
    Fstree(#[from] fstree::Error),

    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },
}

impl From<tokio::task::JoinError> for Error {
    fn from(e: tokio::task::JoinError) -> Error {
        Error::Io {
            context: "copying a layer".into(),
            source: std::io::Error::other(e),
        }
    }
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

/// Where `reference`, an image of Docker Hub, is in the mirror at `mirror`: none for another
/// registry's.
fn mirrored(reference: &Reference, mirror: &str) -> Option<Reference> {
    if reference.registry() != "docker.io" {
        return None;
    }
    let at = match (reference.digest(), reference.tag()) {
        (Some(digest), _) => format!("@{digest}"),
        (None, Some(tag)) => format!(":{tag}"),
        (None, None) => ":latest".to_owned(),
    };
    format!("{mirror}/{}{at}", reference.repository())
        .parse()
        .ok()
}

/// A client of remote OCI registries, with one set of credentials, copying images to and from
/// one store.
///
/// The client under it keeps the credentials and the tokens they earn per registry host,
/// for as long as it lives. So a `RegistryClient` is one caller's: two callers with different
/// credentials sharing one would each be acting with whichever logged in first.
///
/// A multi-platform image resolves to its **linux** manifest whatever the host is, and only
/// the architecture follows the host: the guest that runs the image is Linux even on a Mac.
pub struct RegistryClient {
    store: Store,
    client: Client,
    auth: oci_client::secrets::RegistryAuth,
    /// Docker Hub's mirror, for anonymous pulls ([`RegistryClient::with_mirror`]).
    mirror: Option<String>,
}

impl std::fmt::Debug for RegistryClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegistryClient")
            .field("store", &self.store)
            .field("auth", &self.auth)
            .finish_non_exhaustive()
    }
}

impl RegistryClient {
    /// A client copying to and from `store`, with `auth`, or one that goes anonymously
    /// without it.
    pub fn new(store: Store, auth: Option<RegistryAuth>) -> Self {
        let auth = match auth {
            None => oci_client::secrets::RegistryAuth::Anonymous,
            Some(RegistryAuth::Basic { username, password }) => {
                oci_client::secrets::RegistryAuth::Basic(username, password)
            }
            Some(RegistryAuth::Bearer { token }) => {
                oci_client::secrets::RegistryAuth::Bearer(token)
            }
        };
        RegistryClient {
            store,
            client: Client::new(ClientConfig::default()),
            auth,
            mirror: None,
        }
    }

    /// As [`RegistryClient::new`], Docker Hub's images pulled anonymously from its mirror at
    /// `mirror` first (`mirror.gcr.io`, say): Docker Hub refuses anonymous pulls past a few an
    /// hour from one address, which a server pulling for many reaches. What the mirror does not
    /// have, or will not answer, comes from Docker Hub after all; either way the image is the
    /// one the reference names. None, or credentials -- which are Docker Hub's own, and go
    /// there -- is no mirror: as a client is by default.
    pub fn with_mirror(mut self, mirror: Option<&str>) -> Self {
        let anonymous = matches!(self.auth, oci_client::secrets::RegistryAuth::Anonymous);
        self.mirror = mirror
            .map(|m| m.trim().trim_end_matches('/'))
            .filter(|m| anonymous && !m.is_empty())
            .map(str::to_owned);
        self
    }

    /// `GET /v2/<name>/manifests/<reference>`: the image manifest `reference` names, its
    /// digest, and its bytes as served.
    ///
    /// An index -- one manifest per platform -- is resolved to the linux manifest for the
    /// host's architecture, and fetched in turn. The digest is what `reference` resolved to:
    /// the index's, not that manifest's. Each digest is checked against the bytes it names.
    pub async fn pull_manifest(
        &self,
        reference: &Reference,
    ) -> Result<(Digest, Vec<u8>, ImageManifest)> {
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
        let body = if value.get("manifests").is_some() {
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
                .0
        } else {
            body
        };

        let manifest: ImageManifest =
            serde_json::from_slice(&body).map_err(|e| malformed(format!("its manifest: {e}")))?;
        let digest = digest
            .parse()
            .map_err(|e| malformed(format!("its digest {digest}: {e}")))?;
        Ok((digest, body.into(), manifest))
    }

    /// The config `manifest` names: `GET /v2/<name>/blobs/<digest>`, read as one, and its
    /// bytes as served.
    ///
    /// The manifest names each layer's compressed bytes and the config names its contents,
    /// and nothing but their position pairs the two. A disagreement in how many there are
    /// is an image nothing downstream can read correctly, so it is refused here.
    pub async fn pull_config(
        &self,
        reference: &Reference,
        manifest: &ImageManifest,
    ) -> Result<(Vec<u8>, ImageConfiguration)> {
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
        Ok((blob, config))
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
    /// against the descriptor's digest, as [`pull_blob`](RegistryClient::pull_blob) checks every
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

    /// `PUT /v2/<name>/manifests/<reference>`: put the manifest `body`, of type
    /// `media_type`, under `reference`'s tag as it is, and answer with its digest. Every blob
    /// it names has to be in the repository already.
    pub async fn push_manifest(
        &self,
        reference: &Reference,
        body: Vec<u8>,
        media_type: &MediaType,
    ) -> Result<Digest> {
        self.client
            .store_auth_if_needed(reference.resolve_registry(), &self.auth)
            .await;
        let digest = format!("{:x}", Sha256::digest(&body))
            .parse::<Sha256Digest>()
            .expect("sha2 writes 64 lowercase hex characters")
            .into();
        let content_type = media_type.to_string().parse().map_err(|e| {
            Error::Manifest(
                reference.to_string(),
                format!("its media type {media_type}: {e}"),
            )
        })?;
        self.client
            .push_manifest_raw(reference, body, content_type)
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

    /// Pull the image `reference` names from its registry into the store, and answer with what the
    /// registry resolved `reference` to -- a manifest's digest, or the index's it was chosen from
    /// -- and the image here. Tags nothing.
    ///
    /// A manifest pulled before is the image it became, and a blob pulled before the layer it
    /// became: neither is fetched again. Each line it has to say goes to `report`.
    pub async fn pull(
        &self,
        reference: &Reference,
        mut report: impl FnMut(String) + Send,
    ) -> Result<(Digest, Digest)> {
        report(format!("pulling {reference}"));
        // Docker Hub's image from its mirror, if one is named and has it: the same manifest,
        // byte for byte, so the same digest. Else from Docker Hub.
        let from_mirror = match self.mirror.as_deref().and_then(|m| mirrored(reference, m)) {
            Some(mirrored) => match self.pull_manifest(&mirrored).await {
                Ok(pulled) => Some((mirrored, pulled)),
                Err(e) => {
                    report(format!("not from the mirror ({e}): from Docker Hub"));
                    None
                }
            },
            None => None,
        };
        let (reference, (digest, served, manifest)) = match from_mirror {
            Some((mirrored, pulled)) => (mirrored, pulled),
            None => (reference.clone(), self.pull_manifest(reference).await?),
        };
        let reference = &reference;
        if let Some(image) = self.store.get_image_origin(&digest)? {
            return Ok((digest, image));
        }

        let (config_served, config) = self.pull_config(reference, &manifest).await?;
        let count = manifest.layers().len();
        let mut layers = Vec::new();
        for (index, (descriptor, diff_id)) in manifest
            .layers()
            .iter()
            .zip(config.rootfs().diff_ids())
            .enumerate()
        {
            if let Some(layer) = self.store.get_layer_origin(descriptor.digest())? {
                layers.push(layer);
                continue;
            }
            report(format!(
                "layer {}/{count}: {}",
                index + 1,
                descriptor.digest()
            ));
            let diff_id: Digest = diff_id.parse().map_err(|e| {
                Error::Manifest(reference.to_string(), format!("diff_id {diff_id}: {e}"))
            })?;
            let tree = self.pull_layer(reference, descriptor).await?;
            let layer = {
                let store = self.store.clone();
                tokio::task::spawn_blocking(move || store.insert_layer(&tree)).await??
            };
            self.store
                .insert_layer_origin(descriptor, &diff_id, &layer)?;
            layers.push(layer);
        }

        let image = self.store.insert_image(&config, &layers)?;
        self.store
            .insert_image_origin(&digest, &served, &config_served, &image)?;
        Ok((digest, image))
    }

    /// Push the image `reference` tags in the store to the registry `reference` names, and tag it
    /// there; answer with the digest of the manifest pushed.
    ///
    /// A manifest the image was pulled or pushed as goes again as it was, byte for byte, if the
    /// repository has every layer it names; its config is here to send. Else a manifest is made:
    /// each layer goes as a blob the repository has that an origin says it was, or as a gzipped
    /// archive of it made now, and the config's diff_ids are made to say what those blobs hold.
    /// Each line it has to say goes to `report`.
    pub async fn push(
        &self,
        reference: &Reference,
        mut report: impl FnMut(String) + Send,
    ) -> Result<Digest> {
        report(format!("pushing {reference}"));
        let image = self
            .store
            .get_tag(reference)?
            .ok_or_else(|| Error::Untagged(reference.to_string()))?;
        let (mut config, layers) = self
            .store
            .get_image(&image)?
            .ok_or_else(|| store::Error::Missing(image.clone()))?;

        'origins: for (served, config_served) in self.store.image_origins_of(&image)? {
            let manifest: ImageManifest =
                serde_json::from_slice(&served).map_err(|e| Error::Io {
                    context: format!("reading an origin of {image}"),
                    source: e.into(),
                })?;
            for descriptor in manifest.layers() {
                if !self.blob_exists(reference, descriptor.digest()).await? {
                    continue 'origins;
                }
            }
            let config_digest = manifest.config().digest();
            if !self.blob_exists(reference, config_digest).await? {
                self.push_blob(reference, config_served, config_digest)
                    .await?;
            }
            let media_type = manifest
                .media_type()
                .clone()
                .unwrap_or(MediaType::ImageManifest);
            return self.push_manifest(reference, served, &media_type).await;
        }

        let count = layers.len();
        let mut descriptors = Vec::new();
        let mut diff_ids = Vec::new();
        'layers: for (index, layer) in layers.iter().enumerate() {
            for (descriptor, diff_id) in self.store.layer_origins_of(layer)? {
                if self.blob_exists(reference, descriptor.digest()).await? {
                    descriptors.push(descriptor);
                    diff_ids.push(diff_id.to_string());
                    continue 'layers;
                }
            }
            report(format!("layer {}/{count}: {layer}", index + 1));
            let (blob, diff_id) = {
                let (store, layer) = (self.store.clone(), layer.clone());
                tokio::task::spawn_blocking(move || archive(&store, &layer)).await??
            };
            let digest: Digest = format!("{:x}", Sha256::digest(&blob))
                .parse::<Sha256Digest>()
                .expect("sha2 writes 64 lowercase hex characters")
                .into();
            let descriptor = Descriptor::new(MediaType::ImageLayerGzip, blob.len() as u64, digest);
            self.push_blob(reference, blob, descriptor.digest()).await?;
            self.store
                .insert_layer_origin(&descriptor, &diff_id, layer)?;
            descriptors.push(descriptor);
            diff_ids.push(diff_id.to_string());
        }

        config.rootfs_mut().set_diff_ids(diff_ids);
        let config_body = serde_json::to_vec(&config).map_err(|e| Error::Io {
            context: "serializing a config".into(),
            source: e.into(),
        })?;
        let config_digest: Digest = format!("{:x}", Sha256::digest(&config_body))
            .parse::<Sha256Digest>()
            .expect("sha2 writes 64 lowercase hex characters")
            .into();
        if !self.blob_exists(reference, &config_digest).await? {
            self.push_blob(reference, config_body.clone(), &config_digest)
                .await?;
        }

        let manifest = ImageManifestBuilder::default()
            .schema_version(2u32)
            .media_type(MediaType::ImageManifest)
            .config(Descriptor::new(
                MediaType::ImageConfig,
                config_body.len() as u64,
                config_digest,
            ))
            .layers(descriptors)
            .build()
            .expect("every field a manifest needs is set");
        let body = serde_json::to_vec(&manifest).map_err(|e| Error::Io {
            context: "serializing a manifest".into(),
            source: e.into(),
        })?;
        let digest = self
            .push_manifest(reference, body.clone(), &MediaType::ImageManifest)
            .await?;
        self.store
            .insert_image_origin(&digest, &body, &config_body, &image)?;
        Ok(digest)
    }
}

/// `layer` as a registry takes it: a gzipped archive, and the digest of the archive inside,
/// which is its diff_id.
fn archive(store: &Store, layer: &Digest) -> Result<(Vec<u8>, Digest)> {
    let tree = store
        .get_layer(layer)?
        .ok_or_else(|| store::Error::Missing(layer.clone()))?;
    let mut tar = Vec::new();
    tree.to_changeset(&MediaType::ImageLayer, &mut tar)?;
    let diff_id = format!("{:x}", Sha256::digest(&tar))
        .parse::<Sha256Digest>()
        .expect("sha2 writes 64 lowercase hex characters")
        .into();

    let compressing = |e| Error::Io {
        context: format!("compressing {layer}"),
        source: e,
    };
    let mut gzip = GzEncoder::new(Vec::new(), Compression::default());
    gzip.write_all(&tar).map_err(compressing)?;
    Ok((gzip.finish().map_err(compressing)?, diff_id))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::fstree::{Entry, Meta};

    struct Scratch(Store);

    impl Scratch {
        fn new(name: &str) -> Scratch {
            let path = std::env::temp_dir().join(format!(
                "virtx-uvm-registry-client-{name}-{}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&path);
            Scratch(Store::new(path).unwrap())
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(self.0.root());
        }
    }

    #[test]
    fn credentials_become_the_clients() {
        let scratch = Scratch::new("credentials");
        let registry = RegistryClient::new(
            scratch.0.clone(),
            Some(RegistryAuth::Basic {
                username: "user".into(),
                password: "secret".into(),
            }),
        );
        assert_eq!(
            registry.auth,
            oci_client::secrets::RegistryAuth::Basic("user".into(), "secret".into())
        );
        assert!(!format!("{registry:?}").contains("secret"));

        let registry = RegistryClient::new(
            scratch.0.clone(),
            Some(RegistryAuth::Bearer {
                token: "token".into(),
            }),
        );
        assert_eq!(
            registry.auth,
            oci_client::secrets::RegistryAuth::Bearer("token".into())
        );

        assert_eq!(
            RegistryClient::new(scratch.0.clone(), None).auth,
            oci_client::secrets::RegistryAuth::Anonymous
        );
    }

    #[test]
    fn a_layer_archives_as_what_it_holds() {
        let scratch = Scratch::new("archive");
        let store = &scratch.0;
        let mut tree = FsTree::new();
        tree.insert(
            b"etc/hostname",
            Entry::File {
                meta: Meta::default(),
                data: b"here".to_vec(),
            },
        )
        .unwrap();
        tree.insert(b"gone", Entry::Whiteout).unwrap();
        let layer = store.insert_layer(&tree).unwrap();

        let (blob, diff_id) = archive(store, &layer).unwrap();
        let mut tar = Vec::new();
        std::io::copy(&mut flate2::read::GzDecoder::new(&blob[..]), &mut tar).unwrap();
        assert_eq!(diff_id.digest(), &format!("{:x}", Sha256::digest(&tar)));
        let read = FsTree::from_changeset(&MediaType::ImageLayerGzip, &blob[..]).unwrap();
        assert_eq!(read.digest(), tree.digest());
    }

    #[test]
    fn docker_hub_s_images_are_found_in_its_mirror_by_name_and_no_other_registry_s() {
        let at = |r: &str| mirrored(&r.parse().unwrap(), "mirror.gcr.io").map(|m| m.to_string());
        assert_eq!(
            at("alpine:3.21").as_deref(),
            Some("mirror.gcr.io/library/alpine:3.21")
        );
        assert_eq!(
            at("team/app").as_deref(),
            Some("mirror.gcr.io/team/app:latest")
        );
        let digest = format!("sha256:{}", "a".repeat(64));
        assert_eq!(
            at(&format!("alpine@{digest}")),
            Some(format!("mirror.gcr.io/library/alpine@{digest}"))
        );
        assert_eq!(at("ghcr.io/team/app:v1"), None);
    }

    /// Needs a network: `cargo test -p virtx-uvm -- --ignored`. Docker Hub's image through its
    /// mirror, with Docker Hub named: the same digest, tagged by Docker Hub's name.
    #[tokio::test]
    #[ignore]
    async fn docker_hub_s_image_comes_from_its_mirror() {
        let scratch = Scratch::new("mirror");
        let registry =
            RegistryClient::new(scratch.0.clone(), None).with_mirror(Some("mirror.gcr.io"));
        let reference: Reference = "alpine:3.21".parse().unwrap();
        let mut said = Vec::new();
        let (digest, _) = registry
            .pull(&reference, |line| said.push(line))
            .await
            .unwrap();
        assert_eq!(said[0], "pulling docker.io/library/alpine:3.21");
        let direct = RegistryClient::new(scratch.0.clone(), None);
        let mirrored: Reference = "mirror.gcr.io/library/alpine:3.21".parse().unwrap();
        assert_eq!(direct.pull_manifest(&mirrored).await.unwrap().0, digest);
        // One the mirror has not: from Docker Hub after all -- here, a name no one has, refused.
        let none: Reference = "virtx-no-such-image-anywhere:0".parse().unwrap();
        assert!(registry.pull(&none, |_| {}).await.is_err());
    }

    /// Needs a network: `cargo test -p virtx-uvm -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn pulls_a_public_image() {
        let scratch = Scratch::new("public");
        let registry = RegistryClient::new(scratch.0.clone(), None);
        // Docker Hub's alpine from Google's mirror of it: Docker Hub refuses anonymous pulls
        // past a few an hour, and AWS's mirror past one a second, which says nothing of the pull.
        let reference: Reference = "mirror.gcr.io/library/alpine:3.21".parse().unwrap();
        let (_, _, manifest) = registry.pull_manifest(&reference).await.unwrap();
        let (_, config) = registry.pull_config(&reference, &manifest).await.unwrap();
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

    /// Needs a network: `cargo test -p virtx-uvm -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn a_second_pull_fetches_nothing() {
        let scratch = Scratch::new("pull");
        let store = &scratch.0;
        let registry = RegistryClient::new(store.clone(), None);
        let reference: Reference = "mirror.gcr.io/library/alpine:3.21".parse().unwrap();

        let mut said = Vec::new();
        let (digest, image) = registry
            .pull(&reference, |line| said.push(line))
            .await
            .unwrap();
        assert!(said.len() > 1, "{said:?}");
        assert_eq!(
            store.get_image_origin(&digest).unwrap(),
            Some(image.clone())
        );

        let mut said = Vec::new();
        let again = registry
            .pull(&reference, |line| said.push(line))
            .await
            .unwrap();
        assert_eq!(again, (digest, image));
        assert_eq!(said, vec![format!("pulling {reference}")]);
    }
}
