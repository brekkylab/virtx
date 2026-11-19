//! Room for machines, every sandbox's and build's in this process together, as whoever embeds
//! this crate decides it ([`set_admission`]): a server that holds its machines to what its host
//! can run asks before each boots, and is answered with room held until the machine is down,
//! or why there is none. Without an [`Admission`], every machine boots.

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
    fn admit(&self, spec: &MachineSpec) -> Result<Room, NoRoom>;
}

static ADMISSION: OnceLock<Arc<dyn Admission>> = OnceLock::new();

/// Ask `admission` for room for every machine this process boots, from now on: said once,
/// before any boots. A second is ignored.
pub fn set_admission(admission: Arc<dyn Admission>) {
    let _ = ADMISSION.set(admission);
}

/// Room for a machine of `spec`, as the admission set says: `None`, room asked of no one, where
/// none is set.
pub fn admit(spec: &MachineSpec) -> Result<Option<Room>, NoRoom> {
    match ADMISSION.get() {
        Some(admission) => admission.admit(spec).map(Some),
        None => Ok(None),
    }
}
