use serde::{Deserialize, Serialize};

use super::{Digest, Error, Layer, Reference, Result};

/// The manifest as a registry serves it, holding the config and layer descriptors an
/// image is made of, before any of it has been downloaded.
///
/// Re-exported rather than defined here. Nothing outside this module should name
/// `oci_client`'s own type, so that parsing the manifest ourselves later is a change to
/// this line and not to every caller.
pub use oci_client::manifest::OciImageManifest as ImageManifest;

/// One image. Identified by its manifest digest, not by the reference that found it.
/// Many references can name one image, and an image knows none of them.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Image {
    pub manifest: Digest,

    /// Bottom first.
    pub layers: Vec<Layer>,

    pub config: ImageState,

    /// `os/arch`.
    pub platform: String,
}

impl Image {
    /// Read an image out of what a registry served for it.
    ///
    /// Three inputs and not one, because an image is not in any of them alone: the
    /// manifest says how many layers there are and names their compressed bytes, the
    /// config says what those layers hold and how to run them, and the digest is what
    /// the pair is known by. The two lists disagreeing is the one thing that has to be
    /// caught here, because nothing downstream can.
    pub fn read(
        reference: &Reference,
        manifest: &ImageManifest,
        digest: &Digest,
        parsed: ImageConfig,
    ) -> Result<Image> {
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
            manifest: digest.clone(),
            layers,
            config: ImageState {
                env: stated.env,
                entrypoint: stated.entrypoint,
                cmd: stated.cmd,
                // "" is how a config spells "not set", and `None` is how this does.
                working_dir: stated.working_dir.filter(|w| !w.is_empty()),
                user: stated.user.filter(|u| !u.is_empty()),
            },
            platform: format!("{}/{}", parsed.os, parsed.architecture),
        })
    }
}

/// Only the fields read, so that a registry adding one does not break a pull.
#[derive(Deserialize, Debug, Clone)]
pub struct ImageConfig {
    #[serde(default)]
    architecture: String,

    #[serde(default)]
    os: String,

    #[serde(rename = "config")]
    config: Option<ImageState>,

    rootfs: RootFs,
}

impl ImageConfig {
    /// What the config signs each layer's contents as, bottom first.
    ///
    /// These pair with a manifest's layer descriptors by position and by nothing else,
    /// so a caller that walks both has to have checked that they are the same length.
    pub fn diff_ids(&self) -> &[String] {
        &self.rootfs.diff_ids
    }

    /// `os/arch`, as the config states them.
    pub fn platform(&self) -> String {
        format!("{}/{}", self.os, self.architecture)
    }

    /// What the config says about running the image, with `""` read as "not set".
    pub fn state(&self) -> ImageState {
        let stated = self.config.clone().unwrap_or_default();
        ImageState {
            working_dir: stated.working_dir.filter(|w| !w.is_empty()),
            user: stated.user.filter(|u| !u.is_empty()),
            ..stated
        }
    }

    /// Read one out of the JSON a registry served.
    ///
    /// The error says what is wrong with the JSON and not which image it came from,
    /// because this does not know: naming the image is the caller's, who asked for it.
    pub fn from_json_str(json: &str) -> serde_json::Result<ImageConfig> {
        serde_json::from_str(json)
    }
}

/// What a config says about running the image. Every field is optional because
/// every one of them is optional in the config.
#[derive(Serialize, Deserialize, Default, Debug, Clone)]
pub struct ImageState {
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

#[derive(Deserialize, Debug, Clone)]
struct RootFs {
    #[serde(rename = "diff_ids")]
    diff_ids: Vec<String>,
}

// #[cfg(test)]
// mod tests {
//     use super::*;
//     use oci_client::manifest::OciDescriptor;

//     fn reference(text: &str) -> Reference {
//         text.parse().expect("a reference")
//     }

//     fn descriptor(digest: &str, size: i64) -> OciDescriptor {
//         OciDescriptor {
//             media_type: "application/vnd.oci.image.layer.v1.tar+gzip".to_string(),
//             digest: digest.to_string(),
//             size,
//             ..Default::default()
//         }
//     }

//     fn manifest(layers: Vec<OciDescriptor>) -> ImageManifest {
//         ImageManifest {
//             schema_version: 2,
//             media_type: None,
//             config: descriptor(&Digest::of(b"config").to_string(), 1),
//             layers,
//             ..Default::default()
//         }
//     }

//     #[test]
//     fn a_config_is_read_down_to_what_a_boot_needs() {
//         let blob = Digest::of(b"layer").to_string();
//         let diff_id = Digest::of(b"uncompressed layer").to_string();
//         let config = format!(
//             r#"{{
//                 "architecture": "arm64",
//                 "os": "linux",
//                 "config": {{
//                     "Env": ["PATH=/bin"],
//                     "Entrypoint": ["/bin/sh"],
//                     "WorkingDir": "",
//                     "User": "root"
//                 }},
//                 "rootfs": {{ "type": "layers", "diff_ids": ["{diff_id}"] }},
//                 "history": [{{ "created_by": "nothing here reads this" }}]
//             }}"#
//         );

//         let manifest = manifest(vec![descriptor(&blob, 42)]);
//         let image = Image::read(
//             &reference("alpine:latest"),
//             &manifest,
//             &Digest::of(b"m"),
//             &config,
//         )
//         .unwrap();

//         assert_eq!(image.platform, "linux/arm64");
//         assert_eq!(image.layers.len(), 1);
//         assert_eq!(image.layers[0].digest.to_string(), diff_id);
//         assert_eq!(image.config.env, ["PATH=/bin"]);
//         assert_eq!(image.config.entrypoint, ["/bin/sh"]);
//         assert!(image.config.cmd.is_empty());
//         assert_eq!(image.config.workdir, None);
//         assert_eq!(image.config.user.as_deref(), Some("root"));
//     }

//     #[test]
//     fn a_config_that_disagrees_with_its_manifest_is_refused() {
//         let config = format!(
//             r#"{{"architecture":"arm64","os":"linux","rootfs":{{"type":"layers","diff_ids":["{}"]}}}}"#,
//             Digest::of(b"only one")
//         );
//         let manifest = manifest(vec![
//             descriptor(&Digest::of(b"a").to_string(), 1),
//             descriptor(&Digest::of(b"b").to_string(), 1),
//         ]);

//         let error =
//             Image::read(&reference("x:1"), &manifest, &Digest::of(b"m"), &config).unwrap_err();
//         assert!(
//             matches!(error, Error::Manifest(..)),
//             "expected a manifest error, got {error}"
//         );
//     }
// }
