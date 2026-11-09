//! Sandboxes: a VM booted from an image, with volumes mounted into it and a disk of its own
//! that outlives the VM, made, stopped, started and removed at `/sandboxes/`; and the processes run in one, each a job at
//! `/sandboxes/{id}/jobs/`. Its files are read and written through the volumes it mounts.

#[cfg(feature = "client")]
pub(crate) mod client;
mod job;
mod store;

pub use job::*;
pub use store::*;
