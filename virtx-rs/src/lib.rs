//! A virtx client for Rust, over the C ABI of `libvirtx_core` (`virtx-core/include/virtx.h`).
//!
//! The client is `libvirtx_core`'s: linked into this crate by the `embedded` feature, on by
//! default; or, without it, opened at run time, from the path given to [`load`], or
//! `$VIRTX_CORE_LIB`, or the system loader's search for its file name. Opened, the library is
//! one every module in a process (this crate, the Python or Node package, another native
//! extension) makes its requests through, and a [`Client`] is handed between them as the
//! `VirtxClient*` it is, with [`Client::as_raw`] and [`Client::from_raw`]; so a crate that
//! shares one takes this crate without `embedded`.
//!
//! Each request starts when its method is called, on the library's own runtime, and is a
//! [`Call`] (a `Future`) or [`Events`] (a `Stream`), woken by the library, so any async runtime
//! awaits them; or a thread without one blocks on [`Call::wait`] and [`Events::wait_next`].
//! Starting a job is a [`StartedJob`], a call answering with the job, its [`JobLog`], or its
//! `complete`: a [`JobEnd`], what the job ended with once it has.
//!
//! An app's user reaches the one sandbox they make of a preset with an [`AppClient`].
//!
//! Making or asking for a sandbox or a volume answers a [`Sandbox`] or a [`Volume`], running a
//! process in a sandbox a [`SandboxJob`], and building, pulling or pushing an image an
//! [`ImageJob`]: each carries the client, so it is all a caller hands on to act on it. The two
//! with files reach them through [`Files`].
//!
//! The types requests take and answer are `virtx-core`'s, re-exported here.

mod app_client;
mod call;
mod client;
mod error;
mod files;
mod image;
mod job;
mod library;
mod port_forward;
mod preset;
mod project;
mod sandbox;
mod volume;

pub use app_client::AppClient;
pub use call::{Call, Events};
pub use client::{Client, Opening, ensure_virtx};
pub use error::Error;
pub use files::Files;
pub use image::ImageJob;
pub use job::{JobEnd, JobLog, StartedJob};
pub use library::{LIBRARY_ENV, library_path, load};
pub use port_forward::{Closed, PortForwarder};
pub use preset::{Preset, PresetSandbox};
pub use project::Project;
pub use sandbox::{Sandbox, SandboxJob};
/// What the server answers about an image job; an [`ImageJob`] carries it with its client.
pub use virtx_core::ImageJob as ImageJobInfo;
/// What the server answers about a preset; a [`Preset`] carries it with its client.
pub use virtx_core::Preset as PresetInfo;
/// What the server answers about a project; a [`Project`] carries it with its client.
pub use virtx_core::ProjectListOutputItem as ProjectInfo;
/// What the server answers about a member of a project.
pub use virtx_core::ProjectMemberListOutputItem as ProjectMemberInfo;
/// What the server answers about a sandbox; a [`Sandbox`] carries it with its client.
pub use virtx_core::Sandbox as SandboxInfo;
/// What the server answers about a sandbox job; a [`SandboxJob`] carries it with its client.
pub use virtx_core::SandboxJob as SandboxJobInfo;
/// What the server answers about a volume; a [`Volume`] carries it with its client.
pub use virtx_core::VolumeListOutputItem as VolumeInfo;
pub use virtx_core::{
    Auth, DirEntry, Event, ImageDownloadOutput, ImageJobEnd, ImageJobEntry, ImageJobKind,
    ImageListOutputItem, ImageName, ImagePruneOutput, ImageUploadOutput, LogEntry, Metadata,
    MountSource, MountType, PortSide, PortSpec, PresetAccess, PresetLimits, PresetPutArgs,
    Progress, Recipe, RegistryAuth, Role, SandboxCreateArgs, SandboxExecArgs, SandboxExecItem,
    SandboxJobEnd, SandboxJobEndReason, SandboxJobEntry, SandboxJobKind, SandboxPort,
    SandboxResizeArgs, SandboxResources, SandboxVolume, Source, Step, VolumeMountListOutputItem,
};
pub use volume::Volume;
