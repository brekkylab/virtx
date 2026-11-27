//! A sandbox: a machine booted from an image, with volumes mounted into it, and the processes
//! run in it as [jobs](Job), each with its log.
//!
//! A sandbox made to be kept is kept past its process, its record beside its upper disk, for the
//! next server over its root to know it again ([`Sandbox::kept`]), or for a server it is moved
//! to to take in ([`Sandbox::take_in`]).
//!
//! Where its disk and volumes are kept beyond this machine, its server says with a
//! [`Backing`](crate::backend::Backing): each is taken from there as its machine boots.
//!
//! What a sandbox is to a server beyond this -- how its ports are reached from outside the
//! host -- is the server's.

mod client_ports;
mod error;
mod job;
mod record;
pub mod runs;
#[allow(clippy::module_inception)]
mod sandbox;
mod upper;

pub use client_ports::{ClientConnection, ClientPortTaker};
pub use error::Error;
pub use job::Job;
pub use sandbox::{
    Activity, DEFAULT_DISK_GIB, DEFAULT_MEMORY_MIB, DEFAULT_VCPUS, KEEP_ENDED, Kept, LOG_LIMIT,
    Sandbox, Unknown, park_after, signal_number,
};
