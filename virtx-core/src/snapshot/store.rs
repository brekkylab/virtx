//! Snapshots: the upper layer of a VM's root filesystem, kept by the server under a name, made
//! and removed at `/snapshots/`. Its files are reached at `/snapshots/{name}/files/` with
//! WebDAV.

use serde::{Deserialize, Serialize};

/// A snapshot, as the server answers it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotListOutputItem {
    /// The snapshot's name.
    pub name: String,

    /// The bytes the upper layer takes on the server.
    pub size: u64,

    /// Whether a sandbox holds it now, from its making until its removal. Removing one in use
    /// is refused, and so is every request to its files.
    pub in_use: bool,
}

/// What making a snapshot takes: `POST /snapshots/`'s body.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotCreateArgs {
    /// The name to keep it under. Without one the server picks it. A name already in use is
    /// refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// How much a sandbox standing on it may write over its image, in GiB. Without one the
    /// server picks it; `0` is refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_gib: Option<u32>,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_snapshot_is_read_past_members_it_does_not_know() {
        let value = json!({"name": "dev", "size": 4096, "in_use": false, "cell": "c1"});
        assert_eq!(
            serde_json::from_value::<SnapshotListOutputItem>(value).unwrap(),
            SnapshotListOutputItem {
                name: "dev".into(),
                size: 4096,
                in_use: false,
            }
        );
    }
}
