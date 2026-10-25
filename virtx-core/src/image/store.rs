//! The images the server keeps, and the names it keeps them under.
//!
//! - Digest: the name of an image, the hash of its manifest, as `algorithm:hex` (such as
//!   `sha256:0123abcd...`). It identifies one image's content and never changes.
//! - Reference: a name a person gives an image, as `name:tag` (such as `alpine:3.20`). It points
//!   at one image, and can be moved to another. Any number of references may point at one image.
//!
//! An image no reference points at any more is forgotten, whichever method took its last
//! reference.

use bytes::Bytes;
use serde::{Deserialize, Serialize};

use super::{ImageName, Recipe, RegistryAuth};
use crate::method::{Request, RoundTrip};

/// `image.list`: every reference the server holds, and the image each points at.
pub struct ImageList;

impl Request for ImageList {
    const NAME: &'static str = "image.list";
    type Args = ImageListArgs;
    type Output = Vec<ImageListOutputItem>;
}

/// What `image.list` takes: nothing.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageListArgs {}

/// One reference in what `image.list` answers. Two references to one image are two items with
/// the same digest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageListOutputItem {
    /// The reference, as `name:tag`.
    pub reference: String,

    /// The digest of the image's manifest, as `algorithm:hex`.
    pub digest: String,

    /// The bytes the image's layers take on the server, counting a layer shared with another
    /// image in both.
    pub size: u64,
}

/// `image.pull`: fetch an image from its registry, and point its reference at it.
pub struct ImagePull;

impl Request for ImagePull {
    const NAME: &'static str = "image.pull";
    type Args = ImagePullArgs;
    type Output = ImagePullOutput;
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

/// `image.build`: build a recipe, and point a reference at what it builds.
pub struct ImageBuild;

impl Request for ImageBuild {
    const NAME: &'static str = "image.build";
    type Args = ImageBuildArgs;
    type Output = ImageBuildOutput;
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

/// `image.tag`: point a reference at an image, named by a reference or by its digest.
pub struct ImageTag;

impl Request for ImageTag {
    const NAME: &'static str = "image.tag";
    type Args = ImageTagArgs;
    type Output = ImageTagOutput;
}

/// What `image.tag` takes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageTagArgs {
    /// Which image.
    pub image: ImageName,

    /// The reference to point at it, as `name:tag`. One already in use is moved to this image.
    pub reference: String,
}

/// What `image.tag` answers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageTagOutput {
    /// The digest of the image's manifest, as `algorithm:hex`.
    pub digest: String,
}

/// `image.untag`: take a reference off its image. An image no reference points at any more is
/// forgotten.
pub struct ImageUntag;

impl Request for ImageUntag {
    const NAME: &'static str = "image.untag";
    type Args = ImageUntagArgs;
    type Output = ImageUntagOutput;
}

/// What `image.untag` takes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageUntagArgs {
    /// The reference, as `name:tag`.
    pub reference: String,
}

/// What `image.untag` answers: nothing, once the reference is gone.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageUntagOutput {}

/// `image.remove`: forget an image, named by its digest, and every reference pointing at it.
pub struct ImageRemove;

impl Request for ImageRemove {
    const NAME: &'static str = "image.remove";
    type Args = ImageRemoveArgs;
    type Output = ImageRemoveOutput;
}

/// What `image.remove` takes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageRemoveArgs {
    /// The digest of the image's manifest, as `algorithm:hex`.
    pub digest: String,
}

/// What `image.remove` answers: nothing, once the image is gone.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageRemoveOutput {}

/// `image.export`: send the image a reference points at as an OCI image layout in a tar, the
/// archive in the request's notifications, a piece each in their `content`, with `progress` the
/// bytes sent so far and `total` the archive's length.
pub struct ImageExport;

impl Request for ImageExport {
    const NAME: &'static str = "image.export";
    type Args = ImageExportArgs;
    type Output = ImageExportOutput;
}

/// What `image.export` takes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageExportArgs {
    /// The reference, as `name:tag`.
    pub reference: String,
}

/// What `image.export` answers, once the whole archive is sent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageExportOutput {
    /// The digest of the image's manifest, as `algorithm:hex`.
    pub digest: String,

    /// The archive's length in bytes: what the notifications' pieces add up to.
    pub size: u64,
}

/// `image.import`: take an image as an OCI image layout in a tar, a piece per request, and point
/// a reference at it.
pub struct ImageImport;

impl Request for ImageImport {
    const NAME: &'static str = "image.import";
    type Args = ImageImportArgs;
    type Output = RoundTrip<ImageImportInputRequired, ImageImportOutput>;
}

/// What `image.import` takes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageImportArgs {
    /// The archive's length in bytes, the same in every request of one import.
    pub size: u64,

    /// The next piece of the archive.
    pub data: Bytes,

    /// The `state` of the answer before, exactly as it was given. Without it the request starts
    /// an import.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,

    /// The reference to point at the image, as `name:tag`, the same in every request of one
    /// import.
    ///
    /// A reference already in use is moved to this image.
    pub reference: String,
}

/// What `image.import` answers until it holds the whole archive.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageImportInputRequired {
    /// To send back with the next piece. Only the server reads it.
    pub state: String,

    /// The bytes the server holds.
    pub received: u64,

    /// When the next piece is due, in milliseconds since the Unix epoch, by the server's clock.
    pub due_time: u64,
}

/// What `image.import` answers once the image is kept.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageImportOutput {
    /// The digest of the image's manifest, as `algorithm:hex`.
    pub digest: String,
}
