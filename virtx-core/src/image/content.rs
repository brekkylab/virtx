//! What a client does to the server's images through its OCI registry at `/images/v2/`, not
//! through an RPC method: listing, tagging, removing, and moving an image to and from a file on
//! this machine. The args and outputs are those the C ABI takes and answers.
//!
//! A reference here is the server's: `name:tag`, or `name@algorithm:hex` for an image by its
//! digest, `name` as the server keeps it (`alpine`, `ghcr.io/owner/name`). A file is an OCI
//! image layout in a tar archive, as `skopeo` writes for `oci-archive:`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// A page of the images listing answers, by reference: those after `after` (`name:tag`), by
/// name and then tag, `limit` at most -- as the registry pages its repositories and tags, by
/// `last` and `n`. Every one without either.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageListArgs {
    /// Those after this reference.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,

    /// At most this many.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// One tag in what listing images answers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageListOutputItem {
    /// The tag, as `name:tag`.
    pub reference: String,

    /// The digest of the manifest it points at, as `algorithm:hex`.
    pub digest: String,
}

/// What tagging an image takes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageTagArgs {
    /// The image to tag, as `name:tag` or `name@algorithm:hex`.
    pub source: String,

    /// The tag to point at it, as `name:tag`; one already in use is moved.
    pub target: String,
}

/// What untagging takes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageUntagArgs {
    /// The tag to remove, as `name:tag`. The image it points at stays.
    pub reference: String,
}

/// What removing an image takes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageRemoveArgs {
    /// The image to remove, as `name:tag` or `name@algorithm:hex`. Every tag on it in `name`
    /// goes with it.
    pub reference: String,
}

/// What downloading an image takes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageDownloadArgs {
    /// The image to download, as `name:tag` or `name@algorithm:hex`.
    pub reference: String,

    /// The archive to write, replaced if it exists. Nothing is written there unless the
    /// download succeeds.
    pub path: PathBuf,
}

/// What downloading an image answers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageDownloadOutput {
    /// The digest of the manifest written, as `algorithm:hex`.
    pub digest: String,
}

/// What uploading an image takes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageUploadArgs {
    /// The archive to read.
    pub path: PathBuf,

    /// The tag to point at what is uploaded, as `name:tag`; one already in use is moved. The
    /// archive's only image is uploaded, or the one it names with this tag.
    pub reference: String,
}

/// What uploading an image answers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageUploadOutput {
    /// The digest of the manifest uploaded, as `algorithm:hex`.
    pub digest: String,
}
