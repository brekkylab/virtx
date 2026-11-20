//! Room for machines, every sandbox's and build's in this process together, as whoever embeds
//! this crate decides it ([`set_admission`]): a server that holds its machines to what its host
//! can run asks before each boots, and is answered with room held until the machine is down,
//! or why there is none. Without an [`Admission`], every machine boots.
//!
//! It is asked of the machine's size alone, for the process as a whole: a count of what this
//! host runs, not of whose it is. Nothing tells it the project, the sandbox or the build it is
//! for, nor the disk the sandbox takes, so a limit per project is the embedder's to keep where
//! it knows the project, before it asks this crate for a sandbox.

use std::sync::{Arc, OnceLock};

use crate::MachineSpec;

/// Room a machine was given, held until it drops: as the machine it was asked for goes down.
pub type Room = Box<dyn Send + Sync>;

/// Why a machine was given no room: for the request that asked for it, which a server refuses
/// as the protocol's `503` `capacity` -- nothing was made, and it may be asked again.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct NoRoom(pub String);

/// What decides whether a machine may boot now.
pub trait Admission: Send + Sync {
    /// Room for a machine of `spec` -- a sandbox's, as it is made or boots again after it was
    /// parked, or a build's `RUN` steps' -- held until what this answers drops; refused where
    /// there is none.
    ///
    /// Called on the async runtime, with the sandbox's turn held, and the [`Room`] is dropped
    /// there too, at times with the sandbox's state locked: both are to answer at once, from
    /// what this process knows -- a count in memory -- and never block on anything beyond it.
    fn admit(&self, spec: &MachineSpec) -> Result<Room, NoRoom>;
}

static ADMISSION: OnceLock<Arc<dyn Admission>> = OnceLock::new();

/// Ask `admission` for room for every machine this process boots, from now on: said once,
/// before any boots. `false`, and nothing changed, where one was set already.
pub fn set_admission(admission: Arc<dyn Admission>) -> bool {
    ADMISSION.set(admission).is_ok()
}

/// Room for a machine of `spec`, as the admission set says: `None`, room asked of no one, where
/// none is set.
pub fn admit(spec: &MachineSpec) -> Result<Option<Room>, NoRoom> {
    match ADMISSION.get() {
        Some(admission) => admission.admit(spec).map(Some),
        None => Ok(None),
    }
}
