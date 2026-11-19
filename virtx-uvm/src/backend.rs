//! Where a project's data is kept beyond this machine: a [`StorageBackend`], given to the home
//! that holds the project's stores here, which are its cache. A server with none -- one on its
//! own -- has only those stores, and keeps nothing anywhere else.
//!
//! A home's sandboxes and the server's endpoints call the backend as they make, use and remove
//! a sandbox's disk, a volume or an image, and a refusal of the backend's refuses what they do.
//! What the backend answers in place of the stores here is answered as the protocol has it: a
//! volume it lists is listed as one here is.

use std::sync::{Arc, OnceLock};

use futures_util::future::BoxFuture;
use virtx_core::VolumeListOutputItem;

use crate::{Digest, Reference};

/// What a backend keeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Stored<'a> {
    /// The disk sandbox `id` writes over its image: its upper disk, `sandbox/<id>.ext4` under the
    /// home's root.
    Disk(&'a str),
    /// The volume of this name, `volume/<name>/` under the home's root.
    Volume(&'a str),
}

/// An image a sandbox is about to be made of, before it is looked for here.
#[derive(Clone, Copy, Debug)]
pub enum Wanted<'a> {
    Tag(&'a Reference),
    /// By its digest, as the store or a registry names it.
    Image(&'a Digest),
}

/// What a backend holds for something it let be written here, until it drops.
pub type Held = Box<dyn Send + Sync>;

/// Why a backend refused.
#[derive(Debug, thiserror::Error)]
pub enum Refusal {
    /// It is held beyond this machine, as by another server writing it: answered `409`,
    /// `in_use`, with this message.
    #[error("{0}")]
    InUse(String),
    /// One of its name is kept already: answered `409`, `exists`, with this message.
    #[error("{0}")]
    Exists(String),
    /// Nothing of its name is kept, or here: answered `404`, with this message.
    #[error("{0}")]
    Unknown(String),
    /// The backend failed. Answered `500` with no word of why, which can name what the backend
    /// runs on; the server logs it.
    #[error(transparent)]
    Failed(anyhow::Error),
}

/// Where a project's data is kept beyond this machine, the stores of the project's home here
/// its cache: its sandboxes' disks, its volumes and its images. Each method is called as the
/// home does what it names, and may refuse it; each has a default that keeps nothing beyond the
/// home's own stores, as a home with no backend does.
///
/// What the home has here is used as it is once the backend has let it be: a disk or a volume
/// is brought up to what is kept before it is read or written ([`take`](Self::take),
/// [`bring_up`](Self::bring_up)), and an image before a sandbox is made of it
/// ([`want`](Self::want)). The backend writes into the home's stores itself, under the home's
/// root, as those stores lay them out.
///
/// A sandbox's disk, once taken, is its machine's alone: how the machine reads it is not the
/// backend's to say.
pub trait StorageBackend: Send + Sync {
    /// `what` was just made here, `size` bytes -- the most a disk may grow to; `0` for a volume,
    /// which grows with its files -- before anything is written to it: kept from now on.
    /// Refused [`Exists`](Refusal::Exists) where one of its name is kept already; what was made
    /// here is then removed.
    fn made<'a>(&'a self, what: Stored<'a>, size: u64) -> BoxFuture<'a, Result<(), Refusal>> {
        let _ = (what, size);
        Box::pin(async { Ok(()) })
    }

    /// `what` held here to be written, brought up to what is kept first: a sandbox's disk and
    /// each volume it mounts writable as its machine boots, until the machine is down, and a
    /// volume as a request writes it, until it is answered. Written until the [`Held`] drops,
    /// and to be kept again from then on. Refused [`InUse`](Refusal::InUse) while it is held
    /// beyond this machine, and nothing is written then.
    fn take<'a>(&'a self, what: Stored<'a>) -> BoxFuture<'a, Result<Held, Refusal>> {
        let _ = what;
        Box::pin(async { Ok(Box::new(()) as Held) })
    }

    /// `what` brought up to what is kept, only to be read here: a volume a sandbox is made with,
    /// or mounts read-only as its machine boots, and one a request reads.
    fn bring_up<'a>(&'a self, what: Stored<'a>) -> BoxFuture<'a, Result<(), Refusal>> {
        let _ = what;
        Box::pin(async { Ok(()) })
    }

    /// `what` removed from where it is kept, before anything of it is removed here: a sandbox
    /// deleted, or a volume. Refused [`InUse`](Refusal::InUse) while it is held beyond this
    /// machine, and nothing is removed then, here or there; [`Unknown`](Refusal::Unknown) where
    /// nothing of it is kept, which the server answers as it answers for what it has here. A
    /// sandbox's disk is removed there before its machine is stopped: what was taken of it is
    /// let go of after, its disk gone here too.
    fn remove<'a>(&'a self, what: Stored<'a>) -> BoxFuture<'a, Result<(), Refusal>> {
        let _ = what;
        Box::pin(async { Ok(()) })
    }

    /// Every volume kept, in order by name: what `GET /volumes/` answers in place of the
    /// volumes here, and `GET /volumes/{name}` finds its volume among. `None`, where the volumes
    /// here are all there are.
    fn volumes(&self) -> BoxFuture<'_, anyhow::Result<Option<Vec<VolumeListOutputItem>>>> {
        Box::pin(async { Ok(None) })
    }

    /// The image `wanted` names made ready in the home's image store before a sandbox is made of
    /// it, if it is kept and not here yet: tagged here as it is kept, and the image stored. One
    /// that is not kept is left for the store here to answer for.
    fn want<'a>(&'a self, wanted: Wanted<'a>) -> BoxFuture<'a, anyhow::Result<()>> {
        let _ = wanted;
        Box::pin(async { Ok(()) })
    }
}

/// A home's [`StorageBackend`], once it is given one: shared by the home and each of its
/// sandboxes, which take it as they next need it, and given once.
#[derive(Clone, Default)]
pub struct Backing(Arc<OnceLock<Arc<dyn StorageBackend>>>);

impl Backing {
    /// Back it with `backend` from now on: `false`, and nothing changed, where it was already.
    pub fn set(&self, backend: Arc<dyn StorageBackend>) -> bool {
        self.0.set(backend).is_ok()
    }

    /// The backend, if it has one.
    pub fn get(&self) -> Option<&Arc<dyn StorageBackend>> {
        self.0.get()
    }
}
