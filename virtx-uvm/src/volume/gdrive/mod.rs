//! Google Drive as a read-only backend. [`Gdrive`] documents the tree it serves.

mod accessor;
#[allow(clippy::module_inception)]
mod gdrive;

pub use accessor::GdriveConfig;
// Public because `GdriveConfig::origins` is; `GdriveAccessor` stays private, so outside
// code holds a mount, not a bare client.
pub use accessor::GdriveOrigins;
pub use gdrive::Gdrive;
