//! The machinery behind virtx-uvm: images, the root filesystems made of them, and the
//! sessions that boot them.

pub mod boot;
pub mod cache;
mod file;
pub mod filesystem;
pub mod image;
pub mod machine;
pub mod registry;
pub mod rootfs;
