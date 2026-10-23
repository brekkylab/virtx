use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

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
