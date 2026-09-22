//! OCI images, pulled from a registry and kept on this machine.
//!
//! [`Registry`] is the remote side, [`Cache`] the local one. A pull leaves the
//! registry's own bytes in the cache, being the compressed layer tarballs, the config
//! and the manifest, and nothing is unpacked. Turning those into something a guest can mount is a
//! separate step.

mod cache;
mod digest;
#[allow(clippy::module_inception)]
mod image;
mod registry;

pub use cache::Cache;
pub use digest::Digest;
pub use image::{Image, ImageConfig, ImageManifest};
pub use registry::Registry;

/// One layer of an image.
#[derive(Clone, Debug)]
pub struct Layer {
    /// Digest of the layer uncompressed, called `diff_id` in the OCI config. Names what
    /// the layer contains, whatever a registry happened to compress it with.
    pub digest: Digest,
}

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
