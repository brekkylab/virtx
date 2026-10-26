//! Building images. Every other thing done to the images the server keeps is done through its
//! OCI registry at `/images/v2/`, not through these methods.
//!
//! - Digest: the hash of an image's manifest, as `algorithm:hex` (such as `sha256:0123abcd...`).
//!   It identifies one image's content and never changes.
//! - Reference: a repository and a tag, as `name:tag` (such as `alpine:3.20`). It points at one
//!   manifest, and can be moved to another.

use serde::{Deserialize, Serialize};

use super::Recipe;
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
