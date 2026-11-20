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

use crate::{Digest, ImageStore, ImageStoreError, Reference};

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
///
/// A tag the home writes or removes is told to the backend as it is
/// ([`tagged`](Self::tagged), [`untag`](Self::untag)), and what wrote or removed it is answered
/// once the backend has it so.
///
/// What is kept is the embedder's to remove where no request here does: a project's, as the
/// project is removed, and a sandbox's disk let go of here and taken in nowhere else.
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
    /// and to be kept again from then on: it drops while what it held is still here, before a
    /// closed sandbox's disk is deleted. Refused [`InUse`](Refusal::InUse) while it is held
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
    /// sandbox's disk is removed there before its machine is stopped, so what was taken of it is
    /// let go of after: of something this backend removed, which it keeps nothing of again.
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

    /// The image `wanted` names made ready in the home's image store, if it is kept and not here
    /// yet: tagged here as it is kept, and the image stored. Asked before a sandbox is made of
    /// it, and before the home's registry answers a manifest by its tag or digest. One that is
    /// not kept is left for the store here to answer for.
    fn want<'a>(&'a self, wanted: Wanted<'a>) -> BoxFuture<'a, anyhow::Result<()>> {
        let _ = wanted;
        Box::pin(async { Ok(()) })
    }

    /// The tags kept and not here yet: what the home's registry lists beside its own, in its
    /// catalog and its tag lists, each [wanted](Self::want) once a request names it. A
    /// reference with no tag in it is not listed. Where it fails, the listing is refused `500`
    /// rather than answered with the tags here alone. A tag the registry was pushed or removed
    /// is kept so before that is answered ([`tagged`](Self::tagged), [`untag`](Self::untag)),
    /// so a listing after it lists what it left.
    fn tags(&self) -> BoxFuture<'_, anyhow::Result<Vec<Reference>>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    /// Tag `tag` just written in the home's image store to name `image`, whose manifest, config
    /// and layers are there to be read: kept so from now on, in place of whatever it named.
    /// Told by a push of a manifest by its tag to the home's registry, and by a pull or a build
    /// of the home's, each answered once this is. Where it fails, `tag` is put back here as it
    /// was -- naming what it named, or none -- and the push refused `500`, the pull or build
    /// failed.
    fn tagged<'a>(
        &'a self,
        tag: &'a Reference,
        image: &'a Digest,
    ) -> BoxFuture<'a, anyhow::Result<()>> {
        let _ = (tag, image);
        Box::pin(async { Ok(()) })
    }

    /// Tag `tag` removed from where it is kept: whether it was. Asked by a `DELETE` of the home's
    /// registry, for its tag and for each tag a digest takes away, once it is removed here -- or
    /// at once for a tag not here, its image not [wanted](Self::want) for it, so that a tag whose
    /// image cannot be brought here still goes. The removal is answered once this is: `202`
    /// where the tag was here or kept, `404` where neither. Where it fails, a tag removed here
    /// is put back, and the removal is refused `500`.
    fn untag<'a>(&'a self, tag: &'a Reference) -> BoxFuture<'a, anyhow::Result<bool>> {
        let _ = tag;
        Box::pin(async { Ok(false) })
    }
}

/// A home's [`StorageBackend`], once it is given one: shared by the home and each of its
/// sandboxes, which take it as they next need it, and given once.
#[derive(Clone, Default)]
pub struct Backing(Arc<OnceLock<Arc<dyn StorageBackend>>>);

impl std::fmt::Debug for Backing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let backed = self.get().is_some();
        f.debug_struct("Backing").field("backed", &backed).finish()
    }
}

impl Backing {
    /// Back it with `backend` from now on: `false`, and nothing changed, where it was already.
    pub fn set(&self, backend: Arc<dyn StorageBackend>) -> bool {
        self.0.set(backend).is_ok()
    }

    /// The backend, if it has one.
    pub fn get(&self) -> Option<&Arc<dyn StorageBackend>> {
        self.0.get()
    }

    /// Have `tag` name `image` in `store`, the home's image store, and kept so where there is a
    /// backend ([`StorageBackend::tagged`]): done once both are, and run to its end even if
    /// what awaits it is dropped. Where the backend fails, `tag` is put back here as it was --
    /// naming what it named, or none.
    pub async fn tag(
        &self,
        store: &ImageStore,
        tag: &Reference,
        image: &Digest,
    ) -> Result<(), TagError> {
        let (backing, store, tag, image) =
            (self.clone(), store.clone(), tag.clone(), image.clone());
        let tagging = tokio::spawn(async move {
            let written = (store.clone(), tag.clone(), image.clone());
            let before = blocking(move || {
                let (store, tag, image) = written;
                let before = store.get_tag(&tag)?;
                store.insert_tag(&tag, &image)?;
                Ok::<_, ImageStoreError>(before)
            })
            .await?;
            let Some(backend) = backing.get() else {
                return Ok(());
            };
            let Err(e) = backend.tagged(&tag, &image).await else {
                return Ok(());
            };
            let put_back = blocking(move || match before {
                Some(before) => store.insert_tag(&tag, &before),
                None => store.remove_tag(&tag).map(drop),
            });
            if let Err(put) = put_back.await {
                eprintln!("putting a tag back as it was here: {put}");
            }
            Err(TagError::Kept(e))
        });
        tagging
            .await
            .unwrap_or_else(|e| std::panic::resume_unwind(e.into_panic()))
    }
}

/// Why [`Backing::tag`] left a tag as it was.
#[derive(Debug, thiserror::Error)]
pub enum TagError {
    /// The home's image store failed to write it.
    #[error(transparent)]
    Store(#[from] ImageStoreError),
    /// The backend failed to keep it, and it was put back here. Answered with no word of why,
    /// which can name what the backend runs on; the server logs it.
    #[error(transparent)]
    Kept(anyhow::Error),
}

/// `work`, blocking work, run off the runtime; a panic in it goes on in the caller.
async fn blocking<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
    tokio::task::spawn_blocking(work)
        .await
        .unwrap_or_else(|e| std::panic::resume_unwind(e.into_panic()))
}
