//! Where a project's snapshots, volumes and images are kept past this server, if anywhere: a
//! [`Keeping`] given to its [`Home`] as it is made. A server on its own has none, and each hook
//! is then nothing. virtx cloud's node keeps them in each organization's storage home.

use std::sync::Arc;

use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use virtx_uvm::{Digest, Reference, SnapshotStore, VolumeStore};

use crate::endpoints::Home;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Snapshot,
    Volume,
}

impl Kind {
    pub const ALL: [Kind; 2] = [Kind::Snapshot, Kind::Volume];

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Snapshot => "snapshot",
            Kind::Volume => "volume",
        }
    }

    pub fn plural(self) -> &'static str {
        match self {
            Kind::Snapshot => "snapshots",
            Kind::Volume => "volumes",
        }
    }
}

/// One snapshot or volume, as [`Keeping::list`] answers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Listed {
    pub name: String,
    pub size: u64,
    /// Whether something holds it now.
    pub in_use: bool,
}

/// Refused: held elsewhere, not there, there already, or failed.
#[derive(Debug, thiserror::Error)]
pub enum Refusal {
    #[error("{kind} {name} is in use on node {node}, in {cell}")]
    Held {
        kind: &'static str,
        name: String,
        node: String,
        cell: String,
    },
    #[error("no {kind} is named {name}")]
    Unknown { kind: &'static str, name: String },
    #[error("a {kind} is named {name} already")]
    Exists { kind: &'static str, name: String },
    #[error(transparent)]
    Failed(#[from] anyhow::Error),
}

/// A project's storage past this server. Each hook is called as the server does what it names,
/// and may refuse it; what it is given is the project's home.
pub trait Keeping: Send + Sync {
    /// `kind` `name` was just made here, `disk` bytes: refused if another of that name is
    /// kept already, and then removed here.
    fn made<'a>(
        &'a self,
        kind: Kind,
        name: &'a str,
        disk: u64,
    ) -> BoxFuture<'a, Result<(), Refusal>>;

    /// `kind` `name` held here to be written, brought up to what is kept: refused while
    /// something else holds it.
    fn take<'a>(
        &'a self,
        home: &'a Home,
        kind: Kind,
        name: &'a str,
    ) -> BoxFuture<'a, Result<(), Refusal>>;

    /// As [`take`](Self::take), to boot a machine on: lazily, if the keeping can -- the disk's
    /// data left where it is kept, fetched as the machine reads it ([`chunk`](Self::chunk)).
    fn take_to_boot<'a>(
        &'a self,
        home: &'a Home,
        kind: Kind,
        name: &'a str,
    ) -> BoxFuture<'a, Result<(), Refusal>> {
        self.take(home, kind, name)
    }

    /// The chunk named `chunk` of `kind` `name`, a lazy disk's, as it is kept: its bytes.
    fn chunk<'a>(
        &'a self,
        kind: Kind,
        name: &'a str,
        chunk: &'a str,
    ) -> BoxFuture<'a, anyhow::Result<Vec<u8>>> {
        let _ = chunk;
        Box::pin(async move { anyhow::bail!("{} {name} has no chunks kept", kind.as_str()) })
    }

    /// `kind` `name` brought up to what is kept, only to be read.
    fn bring_up<'a>(
        &'a self,
        home: &'a Home,
        kind: Kind,
        name: &'a str,
    ) -> BoxFuture<'a, Result<(), Refusal>>;

    /// `kind` `name` may have changed here: to be kept again once nothing has it.
    fn changed(&self, kind: Kind, name: &str) -> anyhow::Result<()>;

    /// `kind` `name` removed from where it is kept, before it is here: refused while
    /// something else holds it.
    fn remove<'a>(
        &'a self,
        home: &'a Home,
        kind: Kind,
        name: &'a str,
    ) -> BoxFuture<'a, Result<(), Refusal>>;

    /// What is kept of `kind`, in order by name: those after `after`, `limit` at most -- what
    /// ``GET /snapshots/` and `GET /volumes/` answer instead of what is here.
    fn list<'a>(
        &'a self,
        kind: Kind,
        after: Option<&'a str>,
        limit: Option<u32>,
    ) -> BoxFuture<'a, anyhow::Result<Vec<Listed>>>;

    /// `kind` `name` as it is kept, if it is: what a `GET` of it answers instead of what is
    /// here.
    fn get<'a>(
        &'a self,
        kind: Kind,
        name: &'a str,
    ) -> BoxFuture<'a, anyhow::Result<Option<Listed>>> {
        Box::pin(async move {
            let mut found = self.list(kind, None, None).await?;
            found.retain(|listed| listed.name == name);
            Ok(found.pop())
        })
    }

    /// A request for the project, carrying `token`: what the keeping needs of it.
    fn requested<'a>(&'a self, home: &'a Arc<Home>, token: &'a str) -> BoxFuture<'a, ()>;

    /// Image tag `reference` made ready before it is used, if it is kept and not here yet.
    fn want<'a>(&'a self, home: &'a Home, reference: &'a str) -> BoxFuture<'a, anyhow::Result<()>>;

    /// Image `digest` made ready before it is used, if it is kept and not here yet: as a
    /// sandbox moved here from another node is opened, by the digest it stands on, whatever
    /// tag names it by then. Nothing, where nothing is kept.
    fn want_image<'a>(
        &'a self,
        _home: &'a Home,
        _digest: &'a Digest,
    ) -> BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async { Ok(()) })
    }

    /// The digest and manifest of the image tag `reference` names where it is kept, if it is
    /// not here yet: what a `HEAD` of the tag answers without fetching the image.
    fn wanted_manifest<'a>(
        &'a self,
        reference: &'a str,
    ) -> BoxFuture<'a, anyhow::Result<Option<(Digest, String)>>>;

    /// The manifest of image `digest` where it is kept, if it is: what a `GET` of the manifest
    /// by its digest answers for one not here yet, without fetching the image.
    fn kept_manifest<'a>(
        &'a self,
        _digest: &'a Digest,
    ) -> BoxFuture<'a, anyhow::Result<Option<String>>> {
        Box::pin(async { Ok(None) })
    }

    /// Blob `digest` where it is kept, if it is and is small -- an image's config -- what a
    /// `GET` of it answers for one not here yet. A layer is fetched with its image.
    fn kept_blob<'a>(
        &'a self,
        _digest: &'a Digest,
    ) -> BoxFuture<'a, anyhow::Result<Option<Vec<u8>>>> {
        Box::pin(async { Ok(None) })
    }

    /// The tags kept and not here yet, for the registry to list beside its own.
    fn listed_too(&self) -> Box<dyn Fn() -> Vec<Reference> + Send + Sync>;
}

/// What makes a home's [`Keeping`], given its snapshots and volumes.
pub type MakeKeeping<'a> =
    &'a dyn Fn(&SnapshotStore, &VolumeStore) -> anyhow::Result<Arc<dyn Keeping>>;
