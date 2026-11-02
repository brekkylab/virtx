//! Images, the machines provisioned from them, and the snapshots and registries around
//! them, and the volumes mounted into them -- and the files of a snapshot or a volume, served
//! to a client.

mod files;
mod fstree;
mod image;
mod machine;
mod snapshot;
mod volume;

pub use files::Files;
pub use fstree::*;
pub use image::{
    image_build::{Error as BuildError, build_step},
    registry_client::{Error as RegistryError, Reference, RegistryClient},
    registry_server::{Body as RegistryBody, RegistryServer},
    store::{EROFS_LAYER, Error as ImageStoreError, ImageHold, Pruned, Store as ImageStore},
};
pub use machine::{
    Error as MachineError, Machine, MachineSpec, Network, Ports, Publication,
    Report as MachineReport, SUBCOMMAND as MACHINE_SUBCOMMAND, VolumeMount, enter as enter_machine,
    wire,
};
/// What every image, layer and blob here is named by.
pub use oci_spec::image::Digest;
/// An image's config, and the OS it says it is for.
pub use oci_spec::image::{ImageConfiguration, Os};
pub use snapshot::{
    Error as SnapshotError, Lock as SnapshotLock, Snapshot, Store as SnapshotStore,
};
pub use volume::{
    Dirent, DirentKind, Error as VolumeError, GDRIVE_ORIGINS_ENV, Gdrive, GdriveConfig,
    GdriveOrigins, Hold as VolumeHold, MAX_READ, Mount as RemoteMount, Notion, NotionConfig,
    ONEDRIVE_ORIGINS_ENV, Onedrive, OnedriveConfig, OnedriveOrigins, Remote, Served, Stat,
    Store as VolumeStore, Volume,
};

/// Whether `name` can name a snapshot, a volume or anything else kept under a name: letters,
/// digits, `.`, `_` and `-`, not starting with `.` or `-`, and at most 128 bytes, since it
/// becomes a file name.
pub fn is_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && !name.starts_with(['.', '-'])
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Where virtx keeps what it keeps on this host: `$VIRTX_HOME`, or a `virtx` directory in the
/// platform's cache directory.
pub fn cache_root() -> std::path::PathBuf {
    if let Some(named) = std::env::var_os("VIRTX_HOME") {
        return std::path::PathBuf::from(named);
    }

    #[cfg(windows)]
    return std::env::var_os("LOCALAPPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::PathBuf::from(std::env::var_os("USERPROFILE").unwrap())
                .join("AppData")
                .join("Local")
        })
        .join("virtx");

    #[cfg(target_os = "macos")]
    return std::path::PathBuf::from(std::env::var_os("HOME").unwrap())
        .join("Library")
        .join("Caches")
        .join("virtx");

    #[cfg(not(any(windows, target_os = "macos")))]
    return std::env::var_os("XDG_CACHE_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::PathBuf::from(std::env::var_os("HOME").unwrap()).join(".cache")
        })
        .join("virtx");
}
