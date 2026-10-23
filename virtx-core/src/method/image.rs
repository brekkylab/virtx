//! Images, in the OCI image spec's terms: an image is named by its digest, and any number of
//! references (`name:tag`) point at one.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{image::Recipe, method::Request};

/// `image.list`: every reference the server holds, and the image each points at.
pub struct ImageList;

impl Request for ImageList {
    const NAME: &'static str = "image.list";
    type Args = ImageListArgs;
    type Output = Vec<ImageListOutputItem>;
}

/// What `image.list` takes: nothing.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ImageListArgs {}

/// One reference in what `image.list` answers. Two references to one image are two items with
/// the same digest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
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

/// `image.build`: build a recipe, and point a reference at what it builds.
pub struct ImageBuild;

impl Request for ImageBuild {
    const NAME: &'static str = "image.build";
    type Args = ImageBuildArgs;
    type Output = ImageBuildOutput;
}

/// What `image.build` takes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ImageBuildArgs {
    /// What to build.
    pub recipe: Recipe,

    /// The reference to point at the image, as `name:tag`. Without one the server picks it,
    /// and answers it in [`ImageBuildOutput::reference`].
    ///
    /// A reference already in use is moved to this image.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
}

/// What `image.build` answers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ImageBuildOutput {
    /// The reference pointing at the image: the one asked for, or the one the server picked.
    pub reference: String,

    /// The digest of the image's manifest, as `algorithm:hex`.
    pub digest: String,
}

/// `image.remove`: forget an image, named by a reference or by its digest.
pub struct ImageRemove;

impl Request for ImageRemove {
    const NAME: &'static str = "image.remove";
    type Args = ImageRemoveArgs;
    type Output = ImageRemoveOutput;
}

/// What `image.remove` takes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ImageRemoveArgs {
    /// Which image.
    pub image: ImageName,
}

/// What `image.remove` answers: nothing, once the image is gone.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ImageRemoveOutput {}

/// An image, by a reference to it or by its digest.
///
/// Tagged rather than told apart by spelling, since `sha256:0123abcd` reads as both a digest
/// and the reference `sha256` at tag `0123abcd`.
///
/// ```json
/// {"type": "reference", "reference": "myimg:latest"}
/// {"type": "digest", "digest": "sha256:0123abcd"}
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ImageName {
    /// A reference, as `name:tag`.
    Reference { reference: String },

    /// The digest of the image's manifest, as `algorithm:hex`.
    Digest { digest: String },
}
