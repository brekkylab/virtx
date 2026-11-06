//! A volume, and the client its requests go through.

use virtx_core::{
    FileRoot, MountSource, VolumeListOutputItem as VolumeInfo, VolumeMountListOutputItem,
};

use crate::{Call, Client, Files};

/// A volume the server holds, as making it or asking for it answered, and the client that
/// reaches it: its files, through [`Files`], and the mounts in it.
///
/// Cloning holds the same client once more. Dropping it leaves the volume as it is; it goes
/// with [`remove`](Self::remove), or [`Client::remove_volume`].
#[derive(Clone)]
pub struct Volume {
    client: Client,
    info: VolumeInfo,
}

impl Volume {
    pub(crate) fn new(client: Client, info: VolumeInfo) -> Volume {
        Volume { client, info }
    }

    /// The client its requests go through.
    pub fn client(&self) -> &Client {
        &self.client
    }

    pub fn name(&self) -> &str {
        &self.info.name
    }

    /// What the server answered about it: its size, as it was then.
    pub fn info(&self) -> &VolumeInfo {
        &self.info
    }

    /// Its files, for the client's files requests.
    pub fn root(&self) -> FileRoot {
        FileRoot::Volume(self.info.name.clone())
    }

    /// The mounts in it, without their credentials.
    pub fn mounts(&self) -> Call<Vec<VolumeMountListOutputItem>> {
        self.client.list_volume_mount(&self.info.name)
    }

    /// Mount `source` at `path` in it, a directory the mount creates.
    pub fn mount(
        &self,
        path: impl AsRef<str>,
        source: MountSource,
    ) -> Call<VolumeMountListOutputItem> {
        self.client
            .create_volume_mount(&self.info.name, path, source)
    }

    /// Take away the mount at `path`, leaving its source as it is.
    pub fn unmount(&self, path: impl AsRef<str>) -> Call<()> {
        self.client.remove_volume_mount(&self.info.name, path)
    }

    /// Delete it and everything in it.
    pub fn remove(&self) -> Call<()> {
        self.client.remove_volume(&self.info.name)
    }
}

impl Files for Volume {
    fn client(&self) -> &Client {
        &self.client
    }

    fn root(&self) -> FileRoot {
        Volume::root(self)
    }
}

impl std::fmt::Debug for Volume {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Volume").field(&self.info).finish()
    }
}
