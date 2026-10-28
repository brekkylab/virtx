//! The client's end of `/images/v2/`, the server's OCI registry (OCI Distribution v1.1): what
//! the typed client lists, tags, removes, downloads and uploads images through.
//!
//! An image is a manifest and what it names: an image index names other manifests, an image
//! manifest names its config and layers, all of them blobs. Each operation walks that tree from
//! the manifest a reference points at. Every request is sent over the client's own connection,
//! so it goes wherever `/rpc` does, over either transport.

mod api;
mod archive;
mod images;
mod manifest;
mod reference;
#[cfg(all(test, feature = "server"))]
mod tests;

pub(crate) use self::{
    archive::{download, upload},
    images::{list, remove, tag, untag},
};
