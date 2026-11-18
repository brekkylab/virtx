//! Sandboxes: each a machine booted from an image over a disk of its own, with volumes mounted
//! into it, and the processes run in it as [jobs](Job), each with its log -- from its making
//! until it is deleted, and kept on disk past its server ([`Sandboxes`]).
//!
//! A sandbox's machine is parked once nothing has used it for [`park_after`], and booted again
//! over its disk as it was left for the next job or write to its files -- a read of its files is
//! answered from its disk meanwhile ([`Files`](crate::Files)); a machine that ends
//! on its own leaves its sandbox exited, for good. A sandbox unused for
//! [`reclaim_after`] is deleted ([`Sandboxes::reclaim`]). What a server adds to that -- where
//! else a disk is kept, how many machines it runs at once, how its ports are reached from
//! outside the host -- it plugs in as [`Hooks`].

mod disk;
mod error;
mod hooks;
mod job;
mod parked;
mod record;
pub mod runs;
#[allow(clippy::module_inception)]
mod sandbox;
mod sandboxes;

pub use disk::{cloned, copy_sparse};
pub use error::Error;
pub use hooks::{Held, Hooks, NoHooks, Wanted, Written};
pub use job::Job;
pub(crate) use parked::{Era, Shown};
pub use sandbox::{
    Activity, DEFAULT_MEMORY_MIB, DEFAULT_VCPUS, LABEL_MOST, LOG_LIMIT, MAX_DISK_GIB, Resources,
    Sandbox, park_after, signal_number,
};
pub use sandboxes::{Given, Sandboxes, forget_unrecorded, reclaim_after};
