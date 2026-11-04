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
//! Starting a job is a [`StartedJob`], a call answering with the job, or its [`JobLog`].
//!
//! The types requests take and answer are `virtx-core`'s, re-exported here.

mod call;
mod client;
mod error;
mod job;
mod library;
mod portforward;

pub use call::{Call, Events};
pub use client::{Client, DOMAIN};
pub use error::Error;
pub use job::{JobLog, StartedJob};
pub use library::{LIBRARY_ENV, library_path, load};
pub use portforward::PortForwarder;
pub use virtx_core::{
    Auth, DirEntry, Event, FileRoot, ImageDownloadOutput, ImageJob, ImageJobEnd, ImageJobEntry,
    ImageJobKind, ImageListOutputItem, ImageName, ImagePruneOutput, ImageUploadOutput, LogEntry,
    Metadata, MountSource, MountType, Progress, Recipe, RegistryAuth, Sandbox, SandboxCreateArgs,
    SandboxExecArgs, SandboxExecItem, SandboxJob, SandboxJobEnd, SandboxJobEntry, SandboxJobKind,
    SandboxPort, SandboxState, SandboxVolume, Step, VolumeListOutputItem,
    VolumeMountListOutputItem,
};
