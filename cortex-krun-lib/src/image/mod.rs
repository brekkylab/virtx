//! VM image handling logic.
//!
//! # Terminologies
//!
//! TODO
//!
//! # Operations
//!
//! This module supports `pull`, `provision`, `commit`, `list`, `tag`, `untag` and
//! `remove`.
//!
//! - `pull`: downloads an image from a registry and stores it in the cache
//! - `provision`: prepares a cached image so that it can be booted
//! - `commit`: writes changes back into the local cache
//! - `list`: says what is in the cache and what it costs
//! - `tag`: gives an image another name
//! - `untag`: takes one name away, and nothing else
//! - `remove`: takes every name away, and then what nothing else is using
//! - `prune`: sweeps the whole cache of what no image names at all
//!
//! `untag` and `remove` are two commands rather than one because here they are two
//! different acts. Docker has only `rmi`, which untags when an image has other names and
//! deletes when it does not -- so what it does depends on state the caller did not name.
//! In this cache a name is a key in `images.json` and the files under it are shared by
//! digest, so dropping a name and reclaiming what nothing points at are separable, and
//! separating them is what makes either one safe to run without checking first.
//!
//! See the individual function docs for details on each of them.
//!

mod blob;
mod cache;
mod digest;
mod disk;
mod erofs;
mod ext4;
mod image;
mod layer;
mod ops;
mod registry;
mod stack;
mod tree;

pub use blob::ingest;
pub use cache::{Cache, ImageIndex, Staged};
pub use digest::Digest;
pub use disk::Disk;
pub use erofs::Layout;
pub use erofs::read as read_erofs;
pub use ext4::from_blob as blob_to_ext4;
pub use ext4::to_blob as ext4_to_blob;
pub use image::{Image, ImageConfig, ImageManifest, ImageState};
pub use layer::{Layer, blob_to_erofs, compress_blob, decompress_blob};
pub use ops::{
    GRACE, Listed, Pruned, Removed, commit, list, provision, prune, pull, remove, tag, untag,
};
pub use registry::Registry;
pub use tree::{Directory, Node, Tree};

/// How an image is named, being a registry, a repository, and a tag or a digest.
///
/// Re-exported for the same reason as [`ImageManifest`]. Its `Display` is the canonical
/// spelling, so that `alpine:3.21` and `docker.io/library/alpine:3.21` print the same.
/// That is what makes it, and not a string, what the cache is keyed by.
pub use oci_client::Reference;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// What was handed in as a name did not parse as one. Only what was wrong with
    /// it, not what it was: the conversion takes the name by value, so by the time it
    /// has failed there is nothing left to quote.
    #[error("not an OCI reference: {0}")]
    Reference(String),

    #[error("{0}")]
    Digest(String),

    #[error("pulling {reference}: {source}")]
    Registry {
        reference: String,
        #[source]
        source: Box<oci_client::errors::OciDistributionError>,
    },

    #[error("{0} said something an image cannot say: {1}")]
    Manifest(String, String),

    #[error("{0}")]
    Archive(String),

    /// A session's writable layer could not be read back. Either the filesystem on it is
    /// not one this can read, or what is on it is not what a layer is made of.
    #[error("{0}")]
    Ext4(String),

    #[error("writing an EROFS image: {0}")]
    Erofs(#[source] erofs::Error),

    #[error("reading {0}: {1}")]
    Layer(String, #[source] erofs::Error),

    /// A disk that cannot be put together or cannot be described: a layer that is not
    /// whole blocks, a path a descriptor has no way to spell.
    #[error("{0}")]
    Disk(String),

    #[error("{0} is not in the cache; pull it first")]
    NotPulled(String),

    /// A name the cache does not have, where pulling one is not the answer. Parted from
    /// [`NotPulled`](Self::NotPulled) because that one is advice -- a boot can fetch what
    /// it is missing, and tagging or removing something that is not here cannot.
    #[error("nothing in the cache is named {0}")]
    Unknown(String),

    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },
}

impl Error {
    pub(crate) fn io(context: impl Into<String>, source: std::io::Error) -> Error {
        Error::Io {
            context: context.into(),
            source,
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;
