//! Volumes, and the remote stores mounted in them.
//!
//! * [`Volume`]: storage kept by name, with its files and the stores mounted at paths in it.
//! * [`Remote`]: one of those stores -- [`Notion`], [`Gdrive`] or [`Onedrive`] -- each
//!   read-only and addressed by path.

mod entry;
mod gdrive;
mod notion;
mod onedrive;
mod remote;
mod served;
#[allow(clippy::module_inception)]
mod volume;

pub use entry::{Dirent, DirentKind, Stat};
pub use gdrive::{Gdrive, GdriveConfig, GdriveOrigins};
pub use notion::{Notion, NotionConfig};
pub use onedrive::{Onedrive, OnedriveConfig, OnedriveOrigins};
pub use remote::{GDRIVE_ORIGINS_ENV, ONEDRIVE_ORIGINS_ENV, Remote};
pub use served::Served;
pub use volume::{Error, Hold, MAX_READ, Mount, Store, Volume};

/// The longest name a directory entry may have, in bytes.
pub(crate) const NAME_MAX: u32 = 255;
