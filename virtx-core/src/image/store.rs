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
pub struct ImageJob {
    /// The job's id, which every request about it names it by.
    pub id: String,

    pub kind: ImageJobKind,

    /// The reference it was started with.
    pub reference: String,

    /// Whether it runs still.
    pub running: bool,

    /// Once it has succeeded, as `algorithm:hex`: the digest of the image a pull or build left
    /// here, or of the manifest a push put on the registry.
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
///
/// An entry of a kind a later version of the protocol adds is read as
/// [`Other`](Self::Other), for a reader to pass over, so a log with one still reads.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", remote = "Self")]
#[non_exhaustive]
pub enum ImageJobEntry {
    /// A line of its log.
    Log(String),

    /// The log reached the server's limit: nothing after is kept or sent.
    Truncated {},

    /// How it ended, last.
    End(ImageJobEnd),

    /// One of a kind this version does not know.
    #[serde(skip)]
    Other(crate::UnknownEntry),
}

crate::log_entry::open_entry!(ImageJobEntry);

/// How an image job ended: with a digest if it succeeded, an error if not.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageJobEnd {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// What `POST /images/prune` answers: what it deleted, each image no reference points at and no
/// sandbox uses, and what only such images held.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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

    #[test]
    fn an_entry_of_a_kind_this_version_does_not_know_is_read_to_be_passed_over() {
        let read: crate::LogEntry<ImageJobEntry> =
            serde_json::from_value(json!({"id": 2, "layer": {"pulled": 1}})).unwrap();
        assert_eq!(
            read.entry,
            ImageJobEntry::Other(crate::UnknownEntry {
                kind: "layer".into()
            })
        );
        assert_eq!(read.id, 2);
        for malformed in [
            json!({"id": 2, "log": 5}),
            json!({"id": 2, "truncated": 1}),
            json!({"id": 2, "end": 1}),
        ] {
            let read = serde_json::from_value::<crate::LogEntry<ImageJobEntry>>(malformed.clone());
            assert!(read.is_err(), "{malformed} read as {read:?}");
        }
    }

    #[test]
    fn a_job_its_end_and_a_prune_are_read_past_members_they_do_not_know() {
        let value = json!({
            "id": "j",
            "kind": "pull",
            "reference": "alpine:3.20",
            "running": false,
            "digest": "sha256:00",
            "cell": "c1",
        });
        let job: ImageJob = serde_json::from_value(value).unwrap();
        assert_eq!(job.digest.as_deref(), Some("sha256:00"));

        let value = json!({"end": {"digest": "sha256:00", "cell": "c1"}});
        assert_eq!(
            serde_json::from_value::<ImageJobEntry>(value).unwrap(),
            ImageJobEntry::End(ImageJobEnd {
                digest: Some("sha256:00".into()),
                error: None,
            })
        );

        let value = json!({"images": ["sha256:00"], "reclaimed": 4096, "cell": "c1"});
        assert_eq!(
            serde_json::from_value::<ImagePruneOutput>(value).unwrap(),
            ImagePruneOutput {
                images: vec!["sha256:00".into()],
                reclaimed: 4096,
            }
        );
    }
}
