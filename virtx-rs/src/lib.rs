//! A virtx client for Rust, over the C ABI of `libvirtx_core` (`virtx-core/include/virtx.h`).
//!
//! The client is not compiled in: the library is opened at run time, from the path given to
//! [`load`], or `$VIRTX_CORE_LIB`, or the system loader's search for its file name. So every
//! module in a process (this crate, the Python or Node package, another native extension) can
//! make its requests through the one library, and hand a [`Client`] between them as the
//! `VirtxClient*` it is, with [`Client::as_raw`] and [`Client::from_raw`].
//!
//! Each request starts when its method is called, on the library's own runtime, and is a
//! [`Call`] (a `Future`) or [`Events`] (a `Stream`), woken by the library, so any async runtime
//! awaits them; or a thread without one blocks on [`Call::wait`] and [`Events::wait_next`].
//!
//! The types requests take and answer are `virtx-core`'s, re-exported here.

mod call;
mod client;
mod error;
mod library;

pub use call::{Call, Events};
pub use client::Client;
pub use error::Error;
pub use library::{LIBRARY_ENV, library_path, load};
pub use virtx_core::{
    Event, ImageBuildOutput, ImageName, MountSource, MountType, Progress, Recipe,
    SandboxExecOutput, SandboxInitArgs, SandboxInitOutput, SandboxListOutputItem, SandboxPort,
    SandboxVolume, SnapshotCreateOutput, SnapshotListOutputItem, Step, VolumeCreateOutput,
    VolumeListOutputItem, VolumeMountListOutputItem,
};
