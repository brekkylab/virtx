//! Volumes: storage the server keeps under a name, apart from any VM, and the remote sources
//! mounted in one.

use serde::{Deserialize, Serialize};

use super::{MountSource, MountType};
use crate::method::Request;

/// `volume.list`: every volume the server holds.
pub struct VolumeList;

impl Request for VolumeList {
    const NAME: &'static str = "volume.list";
    type Args = VolumeListArgs;
    type Output = Vec<VolumeListOutputItem>;
}

/// What `volume.list` takes: nothing.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VolumeListArgs {}

/// One volume in what `volume.list` answers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VolumeListOutputItem {
    /// The volume's name.
    pub name: String,

    /// The bytes the volume's files take on the server.
    pub size: u64,
}

/// `volume.create`: keep a new, empty volume under a name.
pub struct VolumeCreate;

impl Request for VolumeCreate {
    const NAME: &'static str = "volume.create";
    type Args = VolumeCreateArgs;
    type Output = VolumeCreateOutput;
}

/// What `volume.create` takes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VolumeCreateArgs {
    /// The name to keep it under. Without one the server picks it, and answers it in
    /// [`VolumeCreateOutput::name`]. A name already in use is refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// What `volume.create` answers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VolumeCreateOutput {
    /// The volume's name: the one asked for, or the one the server picked.
    pub name: String,
}

/// `volume.remove`: delete a volume and everything in it.
pub struct VolumeRemove;

impl Request for VolumeRemove {
    const NAME: &'static str = "volume.remove";
    type Args = VolumeRemoveArgs;
    type Output = VolumeRemoveOutput;
}

/// What `volume.remove` takes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VolumeRemoveArgs {
    /// Which volume, by name.
    pub name: String,
}

/// What `volume.remove` answers: nothing, once the volume is gone.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VolumeRemoveOutput {}

/// `volume.mount.list`: the mounts in a volume.
pub struct VolumeMountList;

impl Request for VolumeMountList {
    const NAME: &'static str = "volume.mount.list";
    type Args = VolumeMountListArgs;
    type Output = Vec<VolumeMountListOutputItem>;
}

/// What `volume.mount.list` takes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VolumeMountListArgs {
    /// Which volume, by name.
    pub volume: String,
}

/// One mount in what `volume.mount.list` answers. Its credentials are not answered.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VolumeMountListOutputItem {
    /// Where it is mounted.
    pub path: String,

    /// What kind of source it is.
    pub mount_type: MountType,
}

/// `volume.mount.create`: show a remote source at a path in a volume. Its files are then
/// reached under that path, at `/volumes/{name}/` and by a VM the volume is mounted into, as
/// the source allows: a change it does not take is refused.
pub struct VolumeMountCreate;

impl Request for VolumeMountCreate {
    const NAME: &'static str = "volume.mount.create";
    type Args = VolumeMountCreateArgs;
    type Output = VolumeMountCreateOutput;
}

/// What `volume.mount.create` takes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VolumeMountCreateArgs {
    /// Which volume, by name.
    pub volume: String,

    /// Where to mount it: a directory the mount creates, not the root, not inside another
    /// mount. Its parent directory must exist and it must not.
    pub path: String,

    pub source: MountSource,
}

/// What `volume.mount.create` answers: nothing, once the mount is there.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VolumeMountCreateOutput {}

/// `volume.mount.remove`: take a mount away, and the directory it was at. The source itself
/// is left as it is.
pub struct VolumeMountRemove;

impl Request for VolumeMountRemove {
    const NAME: &'static str = "volume.mount.remove";
    type Args = VolumeMountRemoveArgs;
    type Output = VolumeMountRemoveOutput;
}

/// What `volume.mount.remove` takes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VolumeMountRemoveArgs {
    /// Which volume, by name.
    pub volume: String,

    /// Where it is mounted.
    pub path: String,
}

/// What `volume.mount.remove` answers: nothing, once the mount is gone.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VolumeMountRemoveOutput {}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_mount_source_is_tagged_by_its_type() {
        let source = MountSource::Gdrive {
            client_id: "id".into(),
            client_secret: "secret".into(),
            refresh_token: "token".into(),
        };
        let value = serde_json::to_value(&source).unwrap();
        assert_eq!(value["type"], "gdrive");
        let decoded: MountSource = serde_json::from_value(value).unwrap();
        assert_eq!(decoded, source);
        assert!(!format!("{source:?}").contains("secret"));
    }

    #[test]
    fn a_onedrive_source_needs_no_client_secret() {
        let value = json!({"type": "onedrive", "client_id": "id", "refresh_token": "token"});
        let decoded: MountSource = serde_json::from_value(value).unwrap();
        assert_eq!(
            decoded,
            MountSource::Onedrive {
                client_id: "id".into(),
                client_secret: None,
                refresh_token: "token".into(),
            }
        );
    }
}
