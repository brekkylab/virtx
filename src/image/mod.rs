//! OCI images, pulled from a registry and turned into something a guest can mount.
//!
//! [`Registry`] is the remote side and [`Cache`] the local one. A [`pull`] goes from one
//! to the other in three steps per layer, none of which keeps what the step before it
//! read: [`decompress_blob`] inflates the blob a registry served, [`blob_to_erofs`]
//! reads that archive and writes an EROFS image, and the cache keeps the image under
//! the layer's `diff_id`. The compressed bytes are never on disk, and the archive is
//! never on disk.
//!
//! What every side of that speaks is a [`Tree`], the shape of a filesystem held in
//! memory: [`ingest`] reads one out of a layer archive and the EROFS writer puts one
//! down. Neither knows about the other.

mod blob;
mod cache;
mod digest;
mod erofs;
mod image;
mod layer;
mod ops;
mod registry;
mod tree;

pub use blob::ingest;
pub use cache::{Cache, ImageIndex, Staged};
pub use digest::Digest;
pub use erofs::Layout;
pub use image::{Image, ImageConfig, ImageManifest, ImageState};
pub use layer::{Layer, blob_to_erofs, decompress_blob};
pub use ops::{provision, pull};
pub use registry::Registry;
pub use tree::Tree;

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
