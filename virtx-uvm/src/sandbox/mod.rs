//! A sandbox: a machine booted from an image, with volumes mounted into it, and the processes
//! run in it as [jobs](Job), each with its log.
//!
//! A sandbox made with keys is kept past its process, its record beside its upper disk, for the
//! next server over its root to know it again ([`Sandbox::kept`]).
//!
//! What a sandbox is to a server beyond this -- how its ports are reached from outside the
//! host -- is the server's.

mod error;
mod job;
mod record;
#[allow(clippy::module_inception)]
mod sandbox;
mod upper;

pub use error::Error;
pub use job::Job;
pub use sandbox::{
    Activity, DEFAULT_DISK_GIB, DEFAULT_MEMORY_MIB, DEFAULT_VCPUS, LOG_LIMIT, Sandbox, park_after,
    signal_number,
};
