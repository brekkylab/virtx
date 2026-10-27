//! Snapshots: the upper layer of a VM's root filesystem, kept by the server under a name. Its
//! files are reached at `/snapshots/{name}/` with WebDAV, not through these methods.

use serde::{Deserialize, Serialize};

use crate::method::Request;

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

    /// Whether a sandbox holds it now, from its `sandbox.init` until its `sandbox.close`.
    /// `snapshot.remove` refuses one in use, and so does every request to its files.
    pub in_use: bool,
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

/// `snapshot.remove`: delete a snapshot and its upper layer. One in use is refused.
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
