//! A snapshot, and the client its requests go through.

use virtx_core::{FileRoot, SnapshotListOutputItem as SnapshotInfo};

use crate::{Call, Client, files};

/// A snapshot the server holds, as making it or asking for it answered, and the client that
/// reaches it: its files, through [`Files`](crate::Files), refused with a `423` while a sandbox holds it.
///
/// Cloning holds the same client once more. Dropping it leaves the snapshot as it is; it goes
/// with [`remove`](Self::remove).
#[derive(Clone)]
pub struct Snapshot {
    client: Client,
    info: SnapshotInfo,
}

impl Snapshot {
    pub(crate) fn new(client: Client, info: SnapshotInfo) -> Snapshot {
        Snapshot { client, info }
    }

    /// The client its requests go through.
    pub fn client(&self) -> &Client {
        &self.client
    }

    pub fn name(&self) -> &str {
        &self.info.name
    }

    /// What the server answered about it: its size and whether a sandbox held it, as they
    /// were then.
    pub fn info(&self) -> &SnapshotInfo {
        &self.info
    }

    /// Delete it and its upper layer; refused with a `409` while a sandbox holds it.
    pub fn remove(&self) -> Call<()> {
        self.client.remove_snapshot(&self.info.name)
    }
}

impl files::sealed::Root for Snapshot {
    fn client(&self) -> &Client {
        &self.client
    }

    fn root(&self) -> FileRoot {
        FileRoot::Snapshot(self.info.name.clone())
    }
}

impl std::fmt::Debug for Snapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Snapshot").field(&self.info).finish()
    }
}
