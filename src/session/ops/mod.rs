//! What a caller actually asks for, spelled out in terms of an [`image`](crate::image)
//! and a machine.
//!
//! This module supports `enter`, and one operation is all it can support: entering a
//! machine is the last thing this process does.

mod enter;

pub use enter::enter;
