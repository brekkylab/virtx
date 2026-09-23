//! EROFS, the read-only filesystem a layer becomes so a guest can mount it.
//!
//! [`write`] puts a [`Tree`](crate::image::tree::Tree) down as an image. It knows
//! nothing about OCI -- whiteouts and layer archives belong to
//! [`crate::image::blob`]. What is here is only the format.

mod format;
mod writer;

pub use writer::{Error, Layout, write};
