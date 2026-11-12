//! Sandboxes: each a machine booted from an image over a disk of its own, with volumes mounted
//! into it, and the processes run in it as [jobs](Job), each with its log -- running or
//! stopped from its making until it is deleted, and kept on disk past its server
//! ([`Sandboxes`]).
//!
//! A sandbox stops after its `auto_stop_minutes` unused ([`Sandboxes::tend`]), or when told
//! to, and starts again over its disk as it was left when told to, or for a job. What a server
//! adds to that -- where else a disk is kept, how many machines it runs at once, how its ports
//! are reached from outside the host -- it plugs in as [`Hooks`].

mod disk;
mod error;
mod hooks;
mod job;
mod record;
#[allow(clippy::module_inception)]
mod sandbox;
mod sandboxes;

pub use disk::{cloned, copy_sparse};
pub use error::Error;
pub use hooks::{Held, Hooks, NoHooks, Wanted, Written};
pub use job::Job;
pub use sandbox::{
    Activity, DEFAULT_MEMORY_MIB, DEFAULT_VCPUS, LABEL_MOST, LOG_LIMIT, Resources, Sandbox,
    Terminal, signal_number,
};
pub use sandboxes::{
    AUTO_DELETE_DAYS, AUTO_STOP_MINUTES, Given, Sandboxes, forget_unrecorded, minute, set_minute,
};
