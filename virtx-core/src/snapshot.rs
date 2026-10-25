//! Snapshots: the upper layer of a VM's root filesystem, an ext4 image, kept by the server under
//! a name.

use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::{
    method::Request,
    volume::{DirEntry, Metadata},
};

/// `snapshot.list`: every snapshot the server holds.
pub struct SnapshotList;

impl Request for SnapshotList {
    const NAME: &'static str = "snapshot.list";
    type Args = SnapshotListArgs;
    type Output = Vec<SnapshotListOutputItem>;
}

/// What `snapshot.list` takes: nothing.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotListArgs {}

/// One snapshot in what `snapshot.list` answers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotListOutputItem {
    /// The snapshot's name.
    pub name: String,

    /// The bytes the upper layer takes on the server.
    pub size: u64,
}

/// `snapshot.create`: keep a new, empty upper layer under a name.
pub struct SnapshotCreate;

impl Request for SnapshotCreate {
    const NAME: &'static str = "snapshot.create";
    type Args = SnapshotCreateArgs;
    type Output = SnapshotCreateOutput;
}

/// What `snapshot.create` takes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotCreateArgs {
    /// The name to keep it under. Without one the server picks it, and answers it in
    /// [`SnapshotCreateOutput::name`]. A name already in use is refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// How much a sandbox standing on it may write over its image, in GiB. Without one the
    /// server picks it; `0` is refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_gib: Option<u32>,
}

/// What `snapshot.create` answers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotCreateOutput {
    /// The snapshot's name: the one asked for, or the one the server picked.
    pub name: String,
}

/// `snapshot.remove`: delete a snapshot and its upper layer.
pub struct SnapshotRemove;

impl Request for SnapshotRemove {
    const NAME: &'static str = "snapshot.remove";
    type Args = SnapshotRemoveArgs;
    type Output = SnapshotRemoveOutput;
}

/// What `snapshot.remove` takes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotRemoveArgs {
    /// Which snapshot, by name.
    pub name: String,
}

/// What `snapshot.remove` answers: nothing, once the snapshot is gone.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotRemoveOutput {}

/// `snapshot.file.stat`: the metadata of a file or directory in a snapshot's upper layer. Only
/// what the upper layer holds: a path only the image under it has is not there.
pub struct SnapshotFileStat;

impl Request for SnapshotFileStat {
    const NAME: &'static str = "snapshot.file.stat";
    type Args = SnapshotFileStatArgs;
    type Output = Metadata;
}

/// What `snapshot.file.stat` takes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotFileStatArgs {
    /// Which snapshot, by name.
    pub snapshot: String,

    /// The file or directory, as a path in the upper layer.
    pub path: String,
}

/// `snapshot.file.read_dir`: the entries of a directory in a snapshot's upper layer, in no set
/// order. Only what the upper layer holds: the image under it is not looked at.
pub struct SnapshotFileReadDir;

impl Request for SnapshotFileReadDir {
    const NAME: &'static str = "snapshot.file.read_dir";
    type Args = SnapshotFileReadDirArgs;
    type Output = Vec<DirEntry>;
}

/// What `snapshot.file.read_dir` takes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotFileReadDirArgs {
    /// Which snapshot, by name.
    pub snapshot: String,

    /// The directory, as a path in the upper layer.
    pub path: String,
}

/// `snapshot.file.read`: bytes of a file in a snapshot's upper layer. Only what the upper layer
/// holds: a file only the image under it has is not there.
pub struct SnapshotFileRead;

impl Request for SnapshotFileRead {
    const NAME: &'static str = "snapshot.file.read";
    type Args = SnapshotFileReadArgs;
    type Output = SnapshotFileReadOutput;
}

/// What `snapshot.file.read` takes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotFileReadArgs {
    /// Which snapshot, by name.
    pub snapshot: String,

    /// The file, as a path in the upper layer.
    pub path: String,

    /// Where to start, from the file's start.
    #[serde(default)]
    pub offset: u64,

    /// At most how many bytes; without it, to the end. A server MAY answer fewer than asked
    /// before the end, so a caller reads on from where the answer stops.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub length: Option<u64>,
}

/// What `snapshot.file.read` answers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotFileReadOutput {
    /// The bytes read: none at or past the end.
    pub data: Bytes,
}
