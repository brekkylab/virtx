//! Images: what the server keeps in its OCI registry at `/images/oci/v2/`, the recipes it
//! builds them from, and the jobs at `/images/jobs/` that build them or copy them between it and
//! other registries.

#[cfg(feature = "client")]
pub(crate) mod client;
mod content;
mod image_name;
#[cfg(feature = "client")]
pub(crate) mod oci;
mod recipe;
mod registry_auth;
mod store;

pub use content::*;
pub use image_name::ImageName;
#[cfg(feature = "client")]
pub(crate) use image_name::is_digest;
pub use recipe::{Recipe, Step};
pub use registry_auth::RegistryAuth;
pub use store::*;
