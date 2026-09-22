use oci_client::client::ClientConfig;
use oci_client::manifest::{IMAGE_MANIFEST_MEDIA_TYPE, ImageIndexEntry, OCI_IMAGE_MEDIA_TYPE};
use oci_client::secrets::RegistryAuth;
use oci_client::{Client, Reference};
use serde::Deserialize;

use super::{Cache, Digest, Error, Image, ImageConfig, ImageManifest, Layer, Result};

/// What a single-platform manifest may be served as.
const MANIFEST_MEDIA_TYPES: &[&str] = &[OCI_IMAGE_MEDIA_TYPE, IMAGE_MANIFEST_MEDIA_TYPE];

/// The remote OCI registry an image is pulled from.
///
/// Always resolves the **linux** image for the host's architecture, never the host's own
/// platform. The thing that runs the image is the guest, and the guest is Linux even on
/// a Mac or on Windows.
///
/// Downloads only. Nothing is unpacked or converted here.
pub struct Registry {
    client: Client,
    auth: RegistryAuth,
}

impl Registry {
    /// A registry client that reads what anybody can read.
    pub fn new() -> Self {
        let config = ClientConfig {
            platform_resolver: Some(Box::new(linux_platform_resolver)),
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

    /// Fetches the manifest for `reference` and the digest that names it.
    ///
    /// The digest is the platform manifest's and not the index's, and only that one
    /// matches what the cache records.
    pub async fn resolve(&self, reference: impl AsRef<str>) -> Result<(ImageManifest, Digest)> {
        let reference = reference.as_ref();
        let parsed = parse(reference)?;
        let (manifest, digest) = self
            .client
            .pull_image_manifest(&parsed, &self.auth)
            .await
            .map_err(|e| Error::Registry {
                reference: reference.to_string(),
                source: Box::new(e),
            })?;

        Ok((manifest, Digest::parse(digest)?))
    }

    /// Put `reference` and everything behind it into `cache`, so that a boot can proceed
    /// offline. Only what is missing is downloaded, and a failed attempt keeps whatever
    /// did arrive.
    pub async fn pull(&self, reference: impl AsRef<str>, cache: &Cache) -> Result<Image> {
        let reference = reference.as_ref();
        let parsed = parse(reference)?;
        let (manifest, digest, config) = self
            .client
            .pull_manifest_and_config(&parsed, &self.auth)
            .await
            .map_err(|e| Error::Registry {
                reference: reference.to_string(),
                source: Box::new(e),
            })?;

        let image = image(reference, &manifest, &digest, &config)?;
        cache.put(config.as_bytes())?;

        for descriptor in &manifest.layers {
            // The compressed blob, which is what a registry serves and verifies. It is
            // not on `Layer`, because nothing outside this loop has any use for it.
            let blob = Digest::parse(&descriptor.digest)?;
            if cache.has(&blob) {
                continue;
            }

            // `pull_blob` verifies against the descriptor as bytes arrive, so nothing
            // here hashes the file again.
            let staged = cache.staging(blob.hex())?;
            let file = tokio::fs::File::create(&staged)
                .await
                .map_err(|e| Error::io(format!("creating {}", staged.display()), e))?;

            let pulled = self.client.pull_blob(&parsed, descriptor, file).await;
            if let Err(e) = pulled {
                let _ = std::fs::remove_file(&staged);
                return Err(Error::Registry {
                    reference: reference.to_string(),
                    source: Box::new(e),
                });
            }

            cache.commit(&staged, &blob)?;
        }

        // Fetched again rather than re-serialised from `manifest`, because a manifest
        // digest is over exact bytes and JSON that has been through a struct is no
        // longer them.
        // Pinned to the resolved digest so a multi-platform reference does not come back
        // as its index.
        let pinned = parsed.clone_with_digest(image.manifest.to_string());
        let (raw, _) = self
            .client
            .pull_manifest_raw(&pinned, &self.auth, MANIFEST_MEDIA_TYPES)
            .await
            .map_err(|e| Error::Registry {
                reference: reference.to_string(),
                source: Box::new(e),
            })?;

        let size = raw.len() as u64;
        let stored = cache.put(&raw)?;
        if stored != image.manifest {
            return Err(Error::Manifest(
                reference.to_string(),
                format!(
                    "its manifest hashes to {stored}, and the registry calls it {}",
                    image.manifest
                ),
            ));
        }

        // Last, so the memo never names an image whose layers did not all arrive. A
        // digest reference is skipped, since it already names its own answer.
        if parsed.digest().is_none() {
            cache.register_reference(
                &parsed,
                &image.manifest,
                manifest
                    .media_type
                    .as_deref()
                    .unwrap_or(OCI_IMAGE_MEDIA_TYPE),
                size,
                Some(&image.platform),
            )?;
        }

        Ok(image)
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

fn parse(reference: &str) -> Result<Reference> {
    reference
        .parse()
        .map_err(|e| Error::Reference(reference.to_string(), e))
}

/// Picks what the guest can run out of a multi-platform image.
fn linux_platform_resolver(manifests: &[ImageIndexEntry]) -> Option<String> {
    let wanted = host_architecture();
    manifests
        .iter()
        .find(|entry| {
            entry.platform.as_ref().is_some_and(|platform| {
                platform.os.to_string() == "linux" && platform.architecture.to_string() == wanted
            })
        })
        .map(|entry| entry.digest.clone())
}

/// The host architecture, spelled the way OCI spells it.
fn host_architecture() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "amd64",
        // Not a host libkrun runs on. Passing the name through keeps the error honest.
        other => other,
    }
}

/// What a registry answered, narrowed to what a boot needs.
fn image(
    reference: &str,
    manifest: &ImageManifest,
    manifest_digest: &str,
    config: &str,
) -> Result<Image> {
    let parsed: ImageConfigJson = serde_json::from_str(config)
        .map_err(|e| Error::Manifest(reference.to_string(), format!("its config: {e}")))?;

    // The two lists describe the same layers and are matched by position alone, so a
    // disagreement here is an image nothing can read correctly.
    if parsed.rootfs.diff_ids.len() != manifest.layers.len() {
        return Err(Error::Manifest(
            reference.to_string(),
            format!(
                "{} layers in the manifest and {} diff_ids in the config",
                manifest.layers.len(),
                parsed.rootfs.diff_ids.len()
            ),
        ));
    }

    let mut layers = Vec::with_capacity(parsed.rootfs.diff_ids.len());
    for diff_id in &parsed.rootfs.diff_ids {
        layers.push(Layer {
            digest: Digest::parse(diff_id)?,
        });
    }

    let stated = parsed.config.unwrap_or_default();
    Ok(Image {
        manifest: Digest::parse(manifest_digest)?,
        layers,
        config: ImageConfig {
            env: stated.env.unwrap_or_default(),
            entrypoint: stated.entrypoint.unwrap_or_default(),
            cmd: stated.cmd.unwrap_or_default(),
            // "" is how a config spells "not set".
            workdir: stated.working_dir.filter(|w| !w.is_empty()),
            user: stated.user.filter(|u| !u.is_empty()),
        },
        platform: format!("{}/{}", parsed.os, parsed.architecture),
    })
}

/// Only the fields read, so that a registry adding one does not break a pull.
#[derive(Deserialize)]
struct ImageConfigJson {
    #[serde(default)]
    architecture: String,
    #[serde(default)]
    os: String,
    #[serde(rename = "config")]
    config: Option<StatedConfig>,
    rootfs: RootFs,
}

#[derive(Deserialize, Default)]
struct StatedConfig {
    #[serde(rename = "Env")]
    env: Option<Vec<String>>,
    #[serde(rename = "Entrypoint")]
    entrypoint: Option<Vec<String>>,
    #[serde(rename = "Cmd")]
    cmd: Option<Vec<String>>,
    #[serde(rename = "WorkingDir")]
    working_dir: Option<String>,
    #[serde(rename = "User")]
    user: Option<String>,
}

#[derive(Deserialize)]
struct RootFs {
    #[serde(rename = "diff_ids")]
    diff_ids: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use oci_client::manifest::OciDescriptor;

    fn descriptor(digest: &str, size: i64) -> OciDescriptor {
        OciDescriptor {
            media_type: "application/vnd.oci.image.layer.v1.tar+gzip".to_string(),
            digest: digest.to_string(),
            size,
            ..Default::default()
        }
    }

    fn manifest(layers: Vec<OciDescriptor>) -> ImageManifest {
        ImageManifest {
            schema_version: 2,
            media_type: None,
            config: descriptor(&Digest::of(b"config").to_string(), 1),
            layers,
            ..Default::default()
        }
    }

    #[test]
    fn a_config_is_read_down_to_what_a_boot_needs() {
        let blob = Digest::of(b"layer").to_string();
        let diff_id = Digest::of(b"uncompressed layer").to_string();
        let config = format!(
            r#"{{
                "architecture": "arm64",
                "os": "linux",
                "config": {{
                    "Env": ["PATH=/bin"],
                    "Entrypoint": ["/bin/sh"],
                    "WorkingDir": "",
                    "User": "root"
                }},
                "rootfs": {{ "type": "layers", "diff_ids": ["{diff_id}"] }},
                "history": [{{ "created_by": "nothing here reads this" }}]
            }}"#
        );

        let manifest = manifest(vec![descriptor(&blob, 42)]);
        let image = image(
            "alpine:latest",
            &manifest,
            &Digest::of(b"m").to_string(),
            &config,
        )
        .unwrap();

        assert_eq!(image.platform, "linux/arm64");
        assert_eq!(image.layers.len(), 1);
        assert_eq!(image.layers[0].digest.to_string(), diff_id);
        assert_eq!(image.config.env, ["PATH=/bin"]);
        assert_eq!(image.config.entrypoint, ["/bin/sh"]);
        assert!(image.config.cmd.is_empty());
        assert_eq!(image.config.workdir, None);
        assert_eq!(image.config.user.as_deref(), Some("root"));
    }

    #[test]
    fn a_config_that_disagrees_with_its_manifest_is_refused() {
        let config = format!(
            r#"{{"architecture":"arm64","os":"linux","rootfs":{{"type":"layers","diff_ids":["{}"]}}}}"#,
            Digest::of(b"only one")
        );
        let manifest = manifest(vec![
            descriptor(&Digest::of(b"a").to_string(), 1),
            descriptor(&Digest::of(b"b").to_string(), 1),
        ]);

        let error = image("x:1", &manifest, &Digest::of(b"m").to_string(), &config).unwrap_err();
        assert!(
            matches!(error, Error::Manifest(..)),
            "expected a manifest error, got {error}"
        );
    }

    #[test]
    fn the_resolver_wants_linux() {
        assert!(matches!(host_architecture(), "arm64" | "amd64"));
        assert_eq!(linux_platform_resolver(&[]), None);
    }
}
