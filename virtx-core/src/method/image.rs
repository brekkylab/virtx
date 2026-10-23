//! Images, in the OCI image spec's terms: an image is named by its digest, and any number of
//! references (`name:tag`) point at one.

use serde::{Deserialize, Serialize};

use crate::{
    image::{ImageName, Recipe},
    method::Request,
};

/// `images.list`: every reference the server holds, and the image each points at.
pub struct ImageList;

impl Request for ImageList {
    const NAME: &'static str = "images.list";
    type Args = ImageListArgs;
    type Output = Vec<ImageListOutputItem>;
}

/// What `images.list` takes: nothing.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageListArgs {}

/// One reference in what `images.list` answers. Two references to one image are two items with
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

/// `images.build`: build a recipe, and point a reference at what it builds.
pub struct ImageBuild;

impl Request for ImageBuild {
    const NAME: &'static str = "images.build";
    type Args = ImageBuildArgs;
    type Output = ImageBuildOutput;
}

/// What `images.build` takes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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

/// What `images.build` answers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageBuildOutput {
    /// The reference pointing at the image: the one asked for, or the one the server picked.
    pub reference: String,

    /// The digest of the image's manifest, as `algorithm:hex`.
    pub digest: String,
}

/// `images.remove`: forget an image, named by a reference or by its digest.
pub struct ImageRemove;

impl Request for ImageRemove {
    const NAME: &'static str = "images.remove";
    type Args = ImageRemoveArgs;
    type Output = ImageRemoveOutput;
}

/// What `images.remove` takes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageRemoveArgs {
    /// Which image.
    pub image: ImageName,
}

/// What `images.remove` answers: nothing, once the image is gone.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageRemoveOutput {}
