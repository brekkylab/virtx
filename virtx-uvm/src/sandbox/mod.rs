//! A sandbox: a machine booted from an image, on a snapshot if one is given, with volumes
//! mounted into it, and the processes run in it as [jobs](Job), each with its log.
//!
//! What a sandbox is to a server beyond this -- the id it is named by, and how its ports are
//! reached from outside the host -- is the server's.

mod error;
mod job;
#[allow(clippy::module_inception)]
mod sandbox;

pub use error::Error;
pub use job::Job;
pub use sandbox::{
    Activity, DEFAULT_MEMORY_MIB, DEFAULT_VCPUS, LOG_LIMIT, Sandbox, park_after, signal_number,
};
