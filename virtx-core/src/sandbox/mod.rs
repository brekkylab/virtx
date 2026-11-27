//! Sandboxes: a VM booted from an image, with volumes mounted into it, made and removed at
//! `/sandboxes/`; and the processes run in one, each a job at `/sandboxes/{id}/jobs/`. Its
//! files are reached at `/sandboxes/{id}/files/` with WebDAV, and each of a client's ports it
//! reaches is taken at `/sandboxes/{id}/client-ports/{port}`.

#[cfg(feature = "client")]
pub(crate) mod client;
mod job;
mod store;

pub use job::*;
pub use store::*;
