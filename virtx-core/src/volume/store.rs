//! Volumes: storage the server keeps under a name, apart from any VM, and the remote sources
//! mounted in one.

use serde::{Deserialize, Serialize};

use super::{MountSource, MountType};

/// A volume, as the server answers it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VolumeListOutputItem {
    /// The volume's name.
    pub name: String,

    /// The bytes the volume's files take on the server.
    pub size: u64,
}

/// What making a volume takes: `POST /volumes/`'s body.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VolumeCreateArgs {
    /// The name to keep it under. Without one the server picks it. A name already in use is
    /// refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// A mount in a volume, as the server answers it. Its credentials are not answered.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VolumeMountListOutputItem {
    /// Where it is mounted.
    pub path: String,

    /// What kind of source it is.
    pub mount_type: MountType,
}

/// What mounting a source takes: `PUT /volumes/{name}/mounts/{path}`'s body. The mount is a
/// directory it creates at that path, not the root, not inside another mount: its parent must
/// exist and it must not. Its files are then reached under that path, as the source allows.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VolumeMountCreateArgs {
    pub source: MountSource,
}

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
