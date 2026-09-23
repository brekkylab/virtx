//! What a caller actually asks for, spelled out in terms of a [`Registry`] and a
//! [`Cache`].
//!
//! [`Registry`](super::Registry) is the wire and [`Cache`](super::Cache) the disk;
//! neither knows about the other. The operations here are where the two meet.

mod commit;
mod provision;
mod pull;

pub use commit::commit;
pub use provision::provision;
pub use pull::pull;
