//! Volumes: storage the server keeps under a name, apart from any VM, and the files in one.
//! Paths in a volume are as [`volume`](crate::volume) says.

use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::{
    method::Request,
    volume::{DirEntry, Metadata, MountSource, MountType},
};

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

/// `volume.file.stat`: a path's metadata.
pub struct VolumeFileStat;

impl Request for VolumeFileStat {
    const NAME: &'static str = "volume.file.stat";
    type Args = VolumeFileStatArgs;
    type Output = Metadata;
}

/// What `volume.file.stat` takes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VolumeFileStatArgs {
    /// Which volume, by name.
    pub volume: String,

    pub path: String,
}

/// `volume.file.read_dir`: a directory's entries, in no set order.
pub struct VolumeFileReadDir;

impl Request for VolumeFileReadDir {
    const NAME: &'static str = "volume.file.read_dir";
    type Args = VolumeFileReadDirArgs;
    type Output = Vec<DirEntry>;
}

/// What `volume.file.read_dir` takes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VolumeFileReadDirArgs {
    /// Which volume, by name.
    pub volume: String,

    /// The directory.
    pub path: String,
}

/// `volume.file.read`: bytes of a file.
pub struct VolumeFileRead;

impl Request for VolumeFileRead {
    const NAME: &'static str = "volume.file.read";
    type Args = VolumeFileReadArgs;
    type Output = VolumeFileReadOutput;
}

/// What `volume.file.read` takes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VolumeFileReadArgs {
    /// Which volume, by name.
    pub volume: String,

    /// The file.
    pub path: String,

    /// Where to start, from the file's start.
    #[serde(default)]
    pub offset: u64,

    /// At most how many bytes; without it, to the end. A server MAY answer fewer than asked
    /// before the end, so a caller reads on from where the answer stops.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub length: Option<u64>,
}

/// What `volume.file.read` answers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VolumeFileReadOutput {
    /// The bytes read: none at or past the end.
    pub data: Bytes,
}

/// `volume.file.write`: write bytes to a file, creating it if there is none.
pub struct VolumeFileWrite;

impl Request for VolumeFileWrite {
    const NAME: &'static str = "volume.file.write";
    type Args = VolumeFileWriteArgs;
    type Output = VolumeFileWriteOutput;
}

/// What `volume.file.write` takes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VolumeFileWriteArgs {
    /// Which volume, by name.
    pub volume: String,

    /// The file. Its parent directory must exist.
    pub path: String,

    /// Where to write, from the file's start, leaving the rest of the file as it was. Without
    /// it the file becomes `data` and nothing else.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<u64>,

    pub data: Bytes,
}

/// What `volume.file.write` answers: nothing, once the bytes are written.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VolumeFileWriteOutput {}

/// `volume.file.create_dir`: create a directory.
pub struct VolumeFileCreateDir;

impl Request for VolumeFileCreateDir {
    const NAME: &'static str = "volume.file.create_dir";
    type Args = VolumeFileCreateDirArgs;
    type Output = VolumeFileCreateDirOutput;
}

/// What `volume.file.create_dir` takes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VolumeFileCreateDirArgs {
    /// Which volume, by name.
    pub volume: String,

    pub path: String,

    /// Also create the missing directories above it, and accept one already there, as
    /// `mkdir -p`. Otherwise its parent must exist and it must not.
    #[serde(default)]
    pub parents: bool,
}

/// What `volume.file.create_dir` answers: nothing, once the directory is there.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VolumeFileCreateDirOutput {}

/// `volume.file.remove`: delete a file or a directory. (`volume.remove`
/// deletes the volume itself.)
pub struct VolumeFileRemove;

impl Request for VolumeFileRemove {
    const NAME: &'static str = "volume.file.remove";
    type Args = VolumeFileRemoveArgs;
    type Output = VolumeFileRemoveOutput;
}

/// What `volume.file.remove` takes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VolumeFileRemoveArgs {
    /// Which volume, by name.
    pub volume: String,

    /// What to delete; not the root.
    pub path: String,

    /// Delete a directory and everything in it. Otherwise a directory must be empty.
    #[serde(default)]
    pub recursive: bool,
}

/// What `volume.file.remove` answers: nothing, once the path is gone.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VolumeFileRemoveOutput {}

/// `volume.file.rename`: move a path to another, within one volume.
pub struct VolumeFileRename;

impl Request for VolumeFileRename {
    const NAME: &'static str = "volume.file.rename";
    type Args = VolumeFileRenameArgs;
    type Output = VolumeFileRenameOutput;
}

/// What `volume.file.rename` takes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VolumeFileRenameArgs {
    /// Which volume, by name.
    pub volume: String,

    pub from: String,

    /// Where it goes. Its parent directory must exist.
    pub to: String,

    /// Replace what is at `to`, as `rename(2)` does. Otherwise a path already there is
    /// refused.
    #[serde(default)]
    pub overwrite: bool,
}

/// What `volume.file.rename` answers: nothing, once the path has moved.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VolumeFileRenameOutput {}

/// `volume.file.copy`: copy a file, or a directory and everything in it, within one volume.
pub struct VolumeFileCopy;

impl Request for VolumeFileCopy {
    const NAME: &'static str = "volume.file.copy";
    type Args = VolumeFileCopyArgs;
    type Output = VolumeFileCopyOutput;
}

/// What `volume.file.copy` takes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VolumeFileCopyArgs {
    /// Which volume, by name.
    pub volume: String,

    pub from: String,

    /// Where the copy goes; not inside `from`. Its parent directory must exist.
    pub to: String,

    /// Delete what is at `to` first, a directory with everything in it. Otherwise a path
    /// already there is refused.
    #[serde(default)]
    pub overwrite: bool,
}

/// What `volume.file.copy` answers: nothing, once the copy is made.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VolumeFileCopyOutput {}

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

    #[serde(rename = "type")]
    pub mount_type: MountType,
}

/// `volume.mount.create`: show a remote source at a path in a volume. Its files are then read
/// through `volume.file.*` and by a VM the volume is mounted into, as the source allows: a
/// change it does not take is refused.
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
    use super::*;

    #[test]
    fn a_mount_source_is_tagged_by_its_type() {
        let source = MountSource::Gdrive {
            client_id: "id".into(),
            client_secret: "secret".into(),
            refresh_token: "token".into(),
        };
        let mut encoded = Vec::new();
        ciborium::into_writer(&source, &mut encoded).unwrap();
        let value: ciborium::Value = ciborium::from_reader(&encoded[..]).unwrap();
        let tag = value
            .as_map()
            .unwrap()
            .iter()
            .find(|(k, _)| k.as_text() == Some("type"))
            .map(|(_, v)| v.as_text());
        assert_eq!(tag, Some(Some("gdrive")));
        let decoded: MountSource = ciborium::from_reader(&encoded[..]).unwrap();
        assert_eq!(decoded, source);
        assert!(!format!("{source:?}").contains("secret"));
    }

    #[test]
    fn a_onedrive_source_needs_no_client_secret() {
        let value = ciborium::Value::Map(vec![
            ("type".into(), "onedrive".into()),
            ("client_id".into(), "id".into()),
            ("refresh_token".into(), "token".into()),
        ]);
        let mut encoded = Vec::new();
        ciborium::into_writer(&value, &mut encoded).unwrap();
        let decoded: MountSource = ciborium::from_reader(&encoded[..]).unwrap();
        assert_eq!(
            decoded,
            MountSource::Onedrive {
                client_id: "id".into(),
                client_secret: None,
                refresh_token: "token".into(),
            }
        );
    }

    #[test]
    fn file_contents_are_a_cbor_byte_string() {
        let args = VolumeFileWriteArgs {
            volume: "v".into(),
            path: "f".into(),
            offset: None,
            data: Bytes::from_static(b"hi"),
        };
        let mut encoded = Vec::new();
        ciborium::into_writer(&args, &mut encoded).unwrap();
        let value: ciborium::Value = ciborium::from_reader(&encoded[..]).unwrap();
        let data = value
            .as_map()
            .unwrap()
            .iter()
            .find(|(k, _)| k.as_text() == Some("data"))
            .map(|(_, v)| v);
        assert_eq!(data, Some(&ciborium::Value::Bytes(b"hi".to_vec())));
        let decoded: VolumeFileWriteArgs = ciborium::from_reader(&encoded[..]).unwrap();
        assert_eq!(decoded, args);
    }

    #[test]
    fn a_read_dir_answer_reads_back() {
        let encoded = {
            let mut bytes = Vec::new();
            let entries = vec![DirEntry {
                name: "a".into(),
                metadata: Metadata {
                    file_type: crate::volume::FileType::Dir,
                    size: 0,
                    modified: 1,
                },
            }];
            ciborium::into_writer(&entries, &mut bytes).unwrap();
            bytes
        };
        let value: ciborium::Value = ciborium::from_reader(&encoded[..]).unwrap();
        let metadata = &value.as_array().unwrap()[0].as_map().unwrap()[1].1;
        let file_type = metadata
            .as_map()
            .unwrap()
            .iter()
            .find(|(k, _)| k.as_text() == Some("type"))
            .map(|(_, v)| v.as_text());
        assert_eq!(file_type, Some(Some("dir")));
        let decoded: Vec<DirEntry> = ciborium::from_reader(&encoded[..]).unwrap();
        assert_eq!(decoded[0].name, "a");
    }
}
