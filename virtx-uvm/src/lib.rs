//! Images, the machines provisioned from them, and the snapshots and registries around
//! them, and the volumes mounted into them; the sandboxes a server makes of those, and the jobs
//! run in each -- and the files of a snapshot, a volume or a sandbox, served to a client.

pub mod disks;
mod files;
mod fstree;
mod image;
mod machine;
pub mod sandbox;
mod snapshot;
mod volume;

pub use files::Files;
pub use fstree::*;
pub use image::{
    image_build::{
        Error as BuildError, RECIPE_LABEL, build_step, history_kept, recipe_forgotten, recipe_kept,
        written_layer,
    },
    registry_client::{
        Error as RegistryError, ProxyPass, ProxyRefusal, PublicImages, Reference, RegistryClient,
        RegistryProxy,
    },
    registry_server::{Body as RegistryBody, RegistryServer},
    store::{EROFS_LAYER, Error as ImageStoreError, ImageHold, Pruned, Store as ImageStore},
};
pub use machine::{
    Error as MachineError, Machine, MachineConfig, MachineSpec, Network, Ports, Publication,
    VolumeMount, enter as enter_machine, wire,
};
/// What every image, layer and blob here is named by.
pub use oci_spec::image::Digest;
/// An image's config, and the OS it says it is for.
pub use oci_spec::image::{ImageConfiguration, Os};
pub use sandbox::{
    Activity, DEFAULT_MEMORY_MIB, DEFAULT_VCPUS, Error as SandboxError, Job as SandboxJob,
    LOG_LIMIT, Sandbox, Sandboxes, signal_number,
};
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

/// `at`, where a volume is mounted in the guest, spelled one way: repeated and trailing `/`
/// dropped. Refused, saying why, unless it is absolute, has no `.` or `..` segment, and is
/// not the guest's root, so that two spellings of one path are one here too. Only the text:
/// a symlink in the guest's tree, which the guest follows when it mounts, is not seen here.
/// Asked of the text rather than of `Path`, which answers by this host's rules: on Windows
/// `/data` is not absolute.
pub(crate) fn mount_point(at: &str) -> std::result::Result<String, &'static str> {
    if !at.starts_with('/') {
        return Err("a path in the guest is absolute");
    }
    let mut spelled = String::new();
    for segment in at.split('/').filter(|segment| !segment.is_empty()) {
        if matches!(segment, "." | "..") {
            return Err("a path in the guest has no . or .. segment");
        }
        spelled.push('/');
        spelled.push_str(segment);
    }
    match spelled.is_empty() {
        true => Err("/ is the guest's root"),
        false => Ok(spelled),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mount_point_is_spelled_one_way_and_refused_where_the_guest_reads_it_otherwise() {
        for (at, spelled) in [
            ("/data", "/data"),
            ("/data/", "/data"),
            ("//data", "/data"),
            ("/a//b/", "/a/b"),
            ("/a.b/..c/.d", "/a.b/..c/.d"),
        ] {
            assert_eq!(mount_point(at).as_deref(), Ok(spelled), "{at}");
        }
        for (at, why) in [
            ("data", "a path in the guest is absolute"),
            ("", "a path in the guest is absolute"),
            ("/", "/ is the guest's root"),
            ("//", "/ is the guest's root"),
            ("/.", "a path in the guest has no . or .. segment"),
            ("/./", "a path in the guest has no . or .. segment"),
            ("/..", "a path in the guest has no . or .. segment"),
            ("/data/.", "a path in the guest has no . or .. segment"),
            ("/data/../etc", "a path in the guest has no . or .. segment"),
        ] {
            assert_eq!(mount_point(at), Err(why), "{at}");
        }
    }
}
