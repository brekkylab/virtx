//! The files in a volume or a snapshot, reached at `/volumes/{name}/` or `/snapshots/{name}/`
//! with WebDAV: what each request names, and what a `PROPFIND` reads into.
//!
//! A path is relative to the root, `/`-separated, as `dir/file.txt`; the empty path is the
//! root. The server refuses one that leads outside.

use serde::{Deserialize, Serialize};

/// Whose files: a volume's, or a snapshot's. As JSON, `{"volume": "name"}` or
/// `{"snapshot": "name"}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileRoot {
    Volume(String),
    Snapshot(String),
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_root_is_tagged_by_its_kind() {
        let root = FileRoot::Volume("v".into());
        assert_eq!(serde_json::to_value(&root).unwrap(), json!({"volume": "v"}));
        let decoded: FileRoot = serde_json::from_value(json!({"snapshot": "s"})).unwrap();
        assert_eq!(decoded, FileRoot::Snapshot("s".into()));
    }

    #[test]
    fn a_path_s_root_is_beside_it() {
        let args = FilePathArgs {
            root: FileRoot::Snapshot("s".into()),
            path: "a/b".into(),
        };
        let value = json!({"snapshot": "s", "path": "a/b"});
        assert_eq!(serde_json::to_value(&args).unwrap(), value);
        assert_eq!(serde_json::from_value::<FilePathArgs>(value).unwrap(), args);
        let read: FileReadArgs =
            serde_json::from_value(json!({"volume": "v", "path": "a", "length": 3})).unwrap();
        assert_eq!((read.offset, read.length), (0, Some(3)));
        let moved: FileMoveArgs =
            serde_json::from_value(json!({"volume": "v", "from": "a", "to": "b"})).unwrap();
        assert!(!moved.overwrite);
        assert!(serde_json::from_value::<FilePathArgs>(json!({"path": "a"})).is_err());
        let written: FileWriteArgs =
            serde_json::from_value(json!({"volume": "v", "path": "a"})).unwrap();
        assert_eq!(written.if_match, None);
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
