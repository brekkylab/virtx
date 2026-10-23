//! Snapshots: the upper layer of a VM's root filesystem, an ext4 image, kept by the server under
//! a name.

use serde::{Deserialize, Serialize};

use crate::method::Request;

/// `snapshots.list`: every snapshot the server holds.
pub struct SnapshotList;

impl Request for SnapshotList {
    const NAME: &'static str = "snapshots.list";
    type Args = SnapshotListArgs;
    type Output = Vec<SnapshotListOutputItem>;
}

/// What `snapshots.list` takes: nothing.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotListArgs {}

/// One snapshot in what `snapshots.list` answers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotListOutputItem {
    /// The snapshot's name.
    pub name: String,

    /// The bytes the upper layer takes on the server.
    pub size: u64,
}

/// `snapshots.create`: keep a new, empty upper layer under a name.
pub struct SnapshotCreate;

impl Request for SnapshotCreate {
    const NAME: &'static str = "snapshots.create";
    type Args = SnapshotCreateArgs;
    type Output = SnapshotCreateOutput;
}

/// What `snapshots.create` takes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotCreateArgs {
    /// The name to keep it under. Without one the server picks it, and answers it in
    /// [`SnapshotCreateOutput::name`]. A name already in use is refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// What `snapshots.create` answers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotCreateOutput {
    /// The snapshot's name: the one asked for, or the one the server picked.
    pub name: String,
}

/// `snapshots.remove`: delete a snapshot and its upper layer.
pub struct SnapshotRemove;

impl Request for SnapshotRemove {
    const NAME: &'static str = "snapshots.remove";
    type Args = SnapshotRemoveArgs;
    type Output = SnapshotRemoveOutput;
}

/// What `snapshots.remove` takes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotRemoveArgs {
    /// Which snapshot, by name.
    pub name: String,
}

/// What `snapshots.remove` answers: nothing, once the snapshot is gone.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotRemoveOutput {}
