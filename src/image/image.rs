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
    pub env: Option<Vec<String>>,

    #[serde(rename = "Entrypoint")]
    pub entrypoint: Option<Vec<String>>,

    #[serde(rename = "Cmd")]
    pub cmd: Option<Vec<String>>,

    #[serde(rename = "WorkingDir")]
    pub working_dir: Option<String>,

    #[serde(rename = "User")]
    pub user: Option<String>,
}

#[derive(Deserialize, Debug, Clone)]
struct RootFs {
    #[serde(rename = "diff_ids")]
    diff_ids: Vec<String>,
}
