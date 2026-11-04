//! Image jobs: building images, and copying them between the server and other registries, each
//! started at `/images/jobs/`. Every other thing done to the images the server keeps is done
//! through its OCI registry at `/images/oci/v2/`.
//!
//! - Digest: the hash of an image's manifest, as `algorithm:hex` (such as `sha256:0123abcd...`).
//!   It identifies one image's content and never changes.
//! - Reference: a repository and a tag, as `name:tag` (such as `alpine:3.20`). It points at one
//!   manifest, and can be moved to another.

use serde::{Deserialize, Serialize};

use super::{Recipe, RegistryAuth};

/// An image job, as the server answers it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageJob {
    /// The job's id, which every request about it names it by.
    pub id: String,

    pub kind: ImageJobKind,

    /// The reference it was started with.
    pub reference: String,

    /// Whether it runs still.
    pub running: bool,

    /// Once it has succeeded, the digest of the manifest it copied or built, as
    /// `algorithm:hex`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,

    /// Once it has failed, why, for a person.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// What an image job does.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageJobKind {
    /// Fetches an image from its registry, and points its reference at it.
    Pull,
    /// Sends an image the server holds to the registry its reference names.
    Push,
    /// Builds a recipe, and points a reference at what it builds.
    Build,
    /// Makes an image of a sandbox -- its image, with what it has written as one more layer --
    /// and points a reference at it.
    Commit,
}

/// What a commit takes: `POST /images/jobs/commit/`'s body.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageCommitArgs {
    /// The sandbox, by id: running, as it is when the job starts, or stopped, as it was left.
    pub sandbox: String,

    /// The reference to point at the image, as `name:tag`.
    ///
    /// A reference already in use is moved to this image.
    pub reference: String,
}

/// What a build takes: `POST /images/jobs/build/`'s body.
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

/// What a pull takes: `POST /images/jobs/pull/`'s body.
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

/// What a push takes: `POST /images/jobs/push/`'s body.
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

/// One entry of an image job's log. As JSON, an object of one member, as `{"log": "..."}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageJobEntry {
    /// A line of its log.
    Log(String),

    /// The log reached the server's limit: nothing after is kept or sent.
    Truncated {},

    /// How it ended, last.
    End(ImageJobEnd),
}

/// How an image job ended: with a digest if it succeeded, an error if not.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageJobEnd {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// What `POST /images/prune` answers: what it deleted, each image no reference points at and no
/// sandbox uses, and what only such images held.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImagePruneOutput {
    /// The digest of each image deleted, as `algorithm:hex`.
    pub images: Vec<String>,

    /// How many bytes the server freed.
    pub reclaimed: u64,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_job_and_its_end_say_only_what_they_have() {
        let job = ImageJob {
            id: "j".into(),
            kind: ImageJobKind::Build,
            reference: "jq:latest".into(),
            running: true,
            digest: None,
            error: None,
        };
        assert_eq!(
            serde_json::to_value(&job).unwrap(),
            json!({"id": "j", "kind": "build", "reference": "jq:latest", "running": true})
        );
        let end = ImageJobEntry::End(ImageJobEnd {
            digest: None,
            error: Some("no image".into()),
        });
        let value = json!({"end": {"error": "no image"}});
        assert_eq!(serde_json::to_value(&end).unwrap(), value);
        assert_eq!(serde_json::from_value::<ImageJobEntry>(value).unwrap(), end);
    }
}
