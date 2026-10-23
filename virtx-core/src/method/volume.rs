//! Volumes: storage the server keeps under a name, apart from any VM.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::method::Request;

/// `volumes.list`: every volume the server holds.
pub struct VolumeList;

impl Request for VolumeList {
    const NAME: &'static str = "volumes.list";
    type Args = VolumeListArgs;
    type Output = Vec<VolumeListOutputItem>;
}

/// What `volumes.list` takes: nothing.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VolumeListArgs {}

/// One volume in what `volumes.list` answers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VolumeListOutputItem {
    /// The volume's name.
    pub name: String,

    /// The bytes the volume's files take on the server.
    pub size: u64,
}

/// `volumes.create`: keep a new, empty volume under a name.
pub struct VolumeCreate;

impl Request for VolumeCreate {
    const NAME: &'static str = "volumes.create";
    type Args = VolumeCreateArgs;
    type Output = VolumeCreateOutput;
}

/// What `volumes.create` takes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VolumeCreateArgs {
    /// The name to keep it under. Without one the server picks it, and answers it in
    /// [`VolumeCreateOutput::name`]. A name already in use is refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// What `volumes.create` answers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VolumeCreateOutput {
    /// The volume's name: the one asked for, or the one the server picked.
    pub name: String,
}

/// `volumes.remove`: delete a volume and everything in it.
pub struct VolumeRemove;

impl Request for VolumeRemove {
    const NAME: &'static str = "volumes.remove";
    type Args = VolumeRemoveArgs;
    type Output = VolumeRemoveOutput;
}

/// What `volumes.remove` takes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VolumeRemoveArgs {
    /// Which volume, by name.
    pub name: String,
}

/// What `volumes.remove` answers: nothing, once the volume is gone.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VolumeRemoveOutput {}
