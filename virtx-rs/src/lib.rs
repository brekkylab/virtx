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
//! Starting a job is a [`StartedJob`], a call answering with the job, its [`JobLog`], or its
//! `complete`: a [`JobEnd`], what the job ended with once it has.
//!
//! Making or asking for a sandbox, a volume or a snapshot answers a [`Sandbox`], a [`Volume`]
//! or a [`Snapshot`], running a process in a sandbox a [`SandboxJob`], and building, pulling or
//! pushing an image an [`ImageJob`]: each carries the client, so it is all a caller hands on to
//! act on it. The three with files reach them through
//! [`Files`].
//!
//! The types requests take and answer are `virtx-core`'s, re-exported here.

mod call;
mod client;
mod error;
mod files;
mod image;
mod job;
mod library;
mod portforward;
mod sandbox;
mod snapshot;
mod volume;

pub use call::{Call, Events};
pub use client::{Client, ensure_virtx};
pub use error::Error;
pub use files::Files;
pub use image::ImageJob;
pub use job::{JobEnd, JobLog, StartedJob};
pub use library::{LIBRARY_ENV, library_path, load};
pub use portforward::PortForwarder;
pub use sandbox::{Sandbox, SandboxJob};
pub use snapshot::Snapshot;
/// What the server answers about an image job; an [`ImageJob`] carries it with its client.
pub use virtx_core::ImageJob as ImageJobInfo;
/// What the server answers about a sandbox; a [`Sandbox`] carries it with its client.
pub use virtx_core::Sandbox as SandboxInfo;
/// What the server answers about a sandbox job; a [`SandboxJob`] carries it with its client.
pub use virtx_core::SandboxJob as SandboxJobInfo;
/// What the server answers about a snapshot; a [`Snapshot`] carries it with its client.
pub use virtx_core::SnapshotListOutputItem as SnapshotInfo;
/// What the server answers about a volume; a [`Volume`] carries it with its client.
pub use virtx_core::VolumeListOutputItem as VolumeInfo;
pub use virtx_core::{
    Auth, DirEntry, Event, ImageDownloadOutput, ImageJobEnd, ImageJobEntry, ImageJobKind,
    ImageListOutputItem, ImageName, ImagePruneOutput, ImageUploadOutput, LogEntry, Metadata,
    MountSource, MountType, Progress, Recipe, RegistryAuth, SandboxCreateArgs, SandboxExecArgs,
    SandboxJobEnd, SandboxJobEntry, SandboxJobKind, SandboxPort, SandboxVolume, Source, Step,
    VolumeMountListOutputItem,
};
pub use volume::Volume;
