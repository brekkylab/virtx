//! Building images, and copying them between the server and other registries. Every other
//! thing done to the images the server keeps is done through its OCI registry at `/images/v2/`,
//! not through these methods.
//!
//! - Digest: the hash of an image's manifest, as `algorithm:hex` (such as `sha256:0123abcd...`).
//!   It identifies one image's content and never changes.
//! - Reference: a repository and a tag, as `name:tag` (such as `alpine:3.20`). It points at one
//!   manifest, and can be moved to another.

use serde::{Deserialize, Serialize};

use super::{Recipe, RegistryAuth};
use crate::method::{Request, StreamingRequest};

/// `image.build`: build a recipe, and point a reference at what it builds.
pub struct ImageBuild;

impl Request for ImageBuild {
    const NAME: &'static str = "image.build";
    type Args = ImageBuildArgs;
    type Output = ImageBuildOutput;
}

impl StreamingRequest for ImageBuild {
    /// A line of the build's log.
    type Content = String;
}

/// What `image.build` takes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageBuildArgs {
    /// What to build.
    pub recipe: Recipe,

    /// The reference to point at the image, as `name:tag`.
    ///
    /// A reference already in use is moved to this image.
    pub reference: String,
}

/// What `image.build` answers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageBuildOutput {
    /// The digest of the image's manifest, as `algorithm:hex`.
    pub digest: String,
}

/// `image.pull`: fetch an image from its registry, and point its reference at it.
pub struct ImagePull;

impl Request for ImagePull {
    const NAME: &'static str = "image.pull";
    type Args = ImagePullArgs;
    type Output = ImagePullOutput;
}

impl StreamingRequest for ImagePull {
    /// A line of the pull's log.
    type Content = String;
}

/// What `image.pull` takes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImagePullArgs {
    /// The image to pull, as its registry names it (`alpine:3.20`, `ghcr.io/owner/name:tag`).
    /// The server holds it under this reference; one already in use is moved to this image.
    pub reference: String,

    /// Credentials for the reference's registry. Without them the server pulls anonymously.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<RegistryAuth>,
}

/// What `image.pull` answers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImagePullOutput {
    /// The digest of the image's manifest, as `algorithm:hex`.
    pub digest: String,
}

/// `image.push`: send an image the server holds to the registry its reference names.
pub struct ImagePush;

impl Request for ImagePush {
    const NAME: &'static str = "image.push";
    type Args = ImagePushArgs;
    type Output = ImagePushOutput;
}

impl StreamingRequest for ImagePush {
    /// A line of the push's log.
    type Content = String;
}

/// What `image.push` takes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImagePushArgs {
    /// The server's reference to push, which also names where it goes
    /// (`ghcr.io/owner/name:tag`). The registry's tag is moved to this image.
    pub reference: String,

    /// Credentials for the reference's registry. Without them the server pushes anonymously.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<RegistryAuth>,
}

/// What `image.push` answers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImagePushOutput {
    /// The digest of the manifest pushed, as `algorithm:hex`.
    pub digest: String,
}

/// `image.prune`: delete each image no reference points at and no sandbox uses, and what only
/// such images held.
pub struct ImagePrune;

impl Request for ImagePrune {
    const NAME: &'static str = "image.prune";
    type Args = ImagePruneArgs;
    type Output = ImagePruneOutput;
}

/// What `image.prune` takes.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImagePruneArgs {}

/// What `image.prune` answers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImagePruneOutput {
    /// The digest of each image deleted, as `algorithm:hex`.
    pub images: Vec<String>,

    /// How many bytes the server freed.
    pub reclaimed: u64,
}
