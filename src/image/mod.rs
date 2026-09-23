//! VM image handling logic.
//!
//! # Terminologies
//!
//! TODO
//!
//! # Operations
//!
//! This module supports `pull`, `provision`, and `commit`.
//!
//! - `pull``: downloads an image from a registry and stores it in the cache
//! - `provision``: prepares a cached image so that it can be booted
//! - `commit``: writes changes back into the local cache
//!
//! See the individual function docs for details on each of them.
//!

mod blob;
mod cache;
mod digest;
mod disk;
mod erofs;
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
pub use image::{Image, ImageConfig, ImageManifest, ImageState};
pub use layer::{Layer, blob_to_erofs, decompress_blob};
pub use ops::{commit, provision, pull};
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
    #[error("{0} is not an OCI reference: {1}")]
    Reference(String, oci_client::ParseError),

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
