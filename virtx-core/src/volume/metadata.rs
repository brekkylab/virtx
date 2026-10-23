use serde::{Deserialize, Serialize};

/// What a path in a volume is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileType {
    File,
    Dir,
}

/// A path's type, size and modification time.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Metadata {
    #[serde(rename = "type")]
    pub file_type: FileType,

    /// The file's length in bytes; for a directory, what the server's filesystem reports.
    pub size: u64,

    /// When the contents last changed, in nanoseconds since the Unix epoch.
    pub modified: i64,
}

/// One entry of a directory: its name within the directory, and its metadata.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirEntry {
    pub name: String,
    pub metadata: Metadata,
}
