//! The files of a volume or a sandbox, which the protocol's Files has a server serve with WebDAV
//! (RFC 4918) under `/volumes/{name}/files/` or `/sandboxes/{id}/files/`: whose they are, and what
//! each call on them names, and what a file or a directory there is, as the C ABI and the packages
//! read and write them.
//!
//! A path is relative to the root, `/`-separated, as `dir/file.txt`; the empty path is the
//! root. The server refuses one that leads outside.

use serde::{Deserialize, Serialize};

/// Whose files: a volume's, or a sandbox's as its processes see them. As JSON,
/// `{"volume": "name"}` or `{"sandbox": "id"}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileRoot {
    Volume(String),
    Sandbox(String),
}

/// What names one path: its root, flattened beside it, and the path. As JSON,
/// `{"volume": "name", "path": "dir/file.txt"}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilePathArgs {
    #[serde(flatten)]
    pub root: FileRoot,
    pub path: String,
}

/// What a read names: a path, and the part of the file to read.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileReadArgs {
    #[serde(flatten)]
    pub root: FileRoot,
    pub path: String,

    /// Where to start, in bytes from the file's start.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub offset: u64,

    /// How many bytes to read; without it, all that is left.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub length: Option<u64>,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// What a write names: a path, and the entity tag the file there has to have still.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileWriteArgs {
    #[serde(flatten)]
    pub root: FileRoot,
    pub path: String,

    /// The `etag` a [`Metadata`] of the file gave: the write is refused with a `412` unless
    /// the file still has it. Without it, the write goes over whatever is there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub if_match: Option<String>,
}

/// What a removal names: a path, and whether nothing being there is fine.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileRemoveArgs {
    #[serde(flatten)]
    pub root: FileRoot,
    pub path: String,

    /// Whether a path with nothing at it is removed already, as `rm -f`, rather than a `404`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub missing_ok: bool,
}

/// What a rename or a copy names: a path, where it goes in the same root, and whether what
/// is there already is replaced.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileMoveArgs {
    #[serde(flatten)]
    pub root: FileRoot,
    pub from: String,
    pub to: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub overwrite: bool,
}

/// What a file or directory is, as a `PROPFIND` answers it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Metadata {
    /// Whether it is a directory (a WebDAV collection).
    pub is_dir: bool,

    /// Its size in bytes; `0` for a directory.
    pub size: u64,

    /// When it was last modified, in seconds since the Unix epoch, if the server says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified: Option<u64>,

    /// Its entity tag as an `ETag` header spells it, quotes and all, if the server gives one:
    /// what a write names as `if_match` to go only over the file it read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
}

/// One entry of a directory, as a `PROPFIND` of depth 1 answers it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirEntry {
    /// Its name in the directory: one path segment.
    pub name: String,

    pub metadata: Metadata,
}

#[cfg(feature = "client")]
impl From<http_over_x::webdav::Meta> for Metadata {
    fn from(meta: http_over_x::webdav::Meta) -> Self {
        let since_epoch = |at: std::time::SystemTime| at.duration_since(std::time::UNIX_EPOCH);
        Metadata {
            is_dir: meta.is_dir,
            size: meta.size,
            modified: meta
                .modified
                .and_then(|at| since_epoch(at).ok())
                .map(|d| d.as_secs()),
            etag: meta.etag,
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_root_is_tagged_by_its_kind() {
        let root = FileRoot::Volume("v".into());
        assert_eq!(serde_json::to_value(&root).unwrap(), json!({"volume": "v"}));
        let decoded: FileRoot = serde_json::from_value(json!({"sandbox": "sb1"})).unwrap();
        assert_eq!(decoded, FileRoot::Sandbox("sb1".into()));
    }

    #[test]
    fn metadata_leaves_out_what_it_lacks() {
        let metadata = Metadata {
            is_dir: true,
            size: 0,
            modified: None,
            etag: None,
        };
        assert_eq!(
            serde_json::to_value(&metadata).unwrap(),
            json!({"is_dir": true, "size": 0})
        );
    }
}
