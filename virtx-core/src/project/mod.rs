//! Projects: what everything a server keeps belongs to, named by each request in
//! [`PROJECT_HEADER`], and made, read and removed at `/projects/`, with their members at
//! `/projects/{name}/members/`, as the Projects section of the protocol has it.

#[cfg(feature = "client")]
pub(crate) mod client;
mod store;

pub use store::*;
