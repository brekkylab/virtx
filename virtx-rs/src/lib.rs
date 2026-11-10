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
//! Making, finding or asking for a sandbox or a volume answers a [`Sandbox`] or a [`Volume`],
//! running a process in a sandbox a [`SandboxJob`], and building, pulling, pushing or
//! committing an image an [`ImageJob`]: each carries the client, so it is all a caller hands on
//! to act on it. The two with files reach them through [`Files`].
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
mod volume;

pub use call::{Call, Events};
pub use client::{Client, DOMAIN, ensure_virtx};
pub use error::Error;
pub use files::Files;
pub use image::ImageJob;
pub use job::{JobLog, StartedJob};
pub use library::{LIBRARY_ENV, library_path, load};
pub use portforward::{ForwardedPort, PortForwarder};
pub use sandbox::{Sandbox, SandboxJob};
/// What the server answers about an image job; an [`ImageJob`] carries it with its client.
pub use virtx_core::ImageJob as ImageJobInfo;
/// What the server answers about a sandbox; a [`Sandbox`] carries it with its client.
pub use virtx_core::Sandbox as SandboxInfo;
/// What the server answers about a sandbox job; a [`SandboxJob`] carries it with its client.
pub use virtx_core::SandboxJob as SandboxJobInfo;
/// What the server answers about a volume; a [`Volume`] carries it with its client.
pub use virtx_core::VolumeListOutputItem as VolumeInfo;
pub use virtx_core::{
    Auth, DirEntry, Event, ImageDownloadOutput, ImageJobEnd, ImageJobEntry, ImageJobKind,
    ImageListOutputItem, ImageName, ImagePruneOutput, ImageUploadOutput, InboundRule, LogEntry,
    Metadata, MountSource, MountType, PortInbound, PortProtocol, PortSpec, Progress, Recipe,
    RegistryAuth, SandboxCreateArgs, SandboxExecArgs, SandboxExecItem, SandboxJobEnd,
    SandboxJobEntry, SandboxJobKind, SandboxPort, SandboxPortOpenArgs, SandboxState, SandboxVolume,
    Step, VolumeMountListOutputItem,
};
pub use volume::Volume;
