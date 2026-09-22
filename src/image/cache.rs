use std::collections::BTreeMap;
use std::fs::{self, File};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{Digest, Error, Reference, Result};

/// The annotation an image layout names a manifest with.
const REF_NAME: &str = "org.opencontainers.image.ref.name";

/// Where to keep images when the caller does not say. `CORTEX_IMAGE_CACHE_ROOT`
/// overrides it.
fn cache_root() -> PathBuf {
    if let Some(named) = std::env::var_os("CORTEX_IMAGE_CACHE_ROOT") {
        return PathBuf::from(named);
    }

    #[cfg(windows)]
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("USERPROFILE")
                .map(|home| PathBuf::from(home).join("AppData").join("Local"))
        });

    #[cfg(not(windows))]
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")));

    base.unwrap_or_else(std::env::temp_dir)
        .join("cortex")
        .join("images")
}

/// Local cache for the VM images downloaded to this machine.
///
/// # Layout
///
/// An OCI image layout, so that `skopeo`, `podman` and anything else that reads the
/// standard can read this directory as it stands:
///
/// ```text
/// <root>/
///   oci-layout           marks the directory as an image layout
///   index.json           one entry per reference pulled here, naming its manifest
///   blobs/sha256/<hex>   layers, configs and manifests, as the registry served them
///   tmp/                 downloads in progress
/// ```
///
/// ## `blobs/`
/// `blobs/` is content-addressed, so it needs no invalidation and a name that exists is
/// bytes that are whole. Nothing lands there except a rename out of `tmp/`.
///
/// ## `index.json`
/// `index.json` is the only part that goes stale, and must, because a tag is a name
/// somebody else can move. It is read and written under a lock, being one file that every pull appends
/// to.
///
/// ```json
/// {
///   "schemaVersion": 2,
///   "mediaType": "application/vnd.oci.image.index.v1+json",
///   "manifests": [
///     {
///       "mediaType": "application/vnd.oci.image.manifest.v1+json",
///       "digest": "sha256:0a7ed0d449b9318548e66674610d757de19b7645759f74b587b610b59d6b43fd",
///       "size": 1025,
///       "annotations": {
///         "org.opencontainers.image.ref.name": "docker.io/library/alpine:3.21"
///       },
///       "platform": {
///         "architecture": "arm64",
///         "os": "linux"
///       }
///     }
///   ]
/// }
/// ```
#[derive(Debug, Clone)]
pub struct Cache {
    root: PathBuf,
}

impl Cache {
    pub fn new() -> Self {
        Cache::at(cache_root())
    }

    pub fn at(root: impl Into<PathBuf>) -> Self {
        Cache { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Where this blob lies, whether or not anything is there.
    pub fn blob(&self, digest: &Digest) -> PathBuf {
        self.root.join("blobs").join(digest.path())
    }

    /// Whether a download can be skipped.
    pub fn has(&self, digest: &Digest) -> bool {
        self.blob(digest).is_file()
    }

    pub fn read(&self, digest: &Digest) -> Result<Vec<u8>> {
        let path = self.blob(digest);
        fs::read(&path).map_err(|e| Error::io(format!("reading {}", path.display()), e))
    }

    /// Keep bytes already in hand, such as a manifest or a config. A layer goes through
    /// [`staging`](Self::staging) and [`commit`](Self::commit) so it never has to be in
    /// memory.
    pub fn put(&self, bytes: &[u8]) -> Result<Digest> {
        let digest = Digest::of(bytes);
        if self.has(&digest) {
            return Ok(digest);
        }

        let staged = self.staging(digest.hex())?;
        fs::write(&staged, bytes)
            .map_err(|e| Error::io(format!("writing {}", staged.display()), e))?;
        self.commit(&staged, &digest)?;
        Ok(digest)
    }

    /// Somewhere to write a download to. Keeps half-finished bytes out of the way of
    /// anything looking for a finished blob.
    pub fn staging(&self, hint: &str) -> Result<PathBuf> {
        let tmp = self.root.join("tmp");
        fs::create_dir_all(&tmp).map_err(|e| Error::io(format!("making {}", tmp.display()), e))?;
        Ok(tmp.join(format!("{}.{hint}.part", std::process::id())))
    }

    /// Publish a finished download, so that from here on [`has`](Self::has) answers yes.
    pub fn commit(&self, staged: &Path, digest: &Digest) -> Result<PathBuf> {
        let kept = self.blob(digest);
        if kept.is_file() {
            let _ = fs::remove_file(staged);
            return Ok(kept);
        }

        let parent = kept.parent().expect("a blob path has a directory");
        fs::create_dir_all(parent)
            .map_err(|e| Error::io(format!("making {}", parent.display()), e))?;
        fs::rename(staged, &kept).map_err(|e| {
            Error::io(
                format!("moving {} to {}", staged.display(), kept.display()),
                e,
            )
        })?;
        Ok(kept)
    }

    /// What `reference` last resolved to, for deciding whether a tag has moved. A memo,
    /// not an answer, because only the registry knows what a tag means now.
    pub fn resolved(&self, reference: &Reference) -> Option<Digest> {
        let name = reference.to_string();
        self.index()
            .ok()?
            .manifests
            .iter()
            .find(|entry| entry.annotations.get(REF_NAME) == Some(&name))
            .and_then(|entry| Digest::parse(&entry.digest).ok())
    }

    /// Register about the `reference` to the `index.json`.
    pub fn register_reference(
        &self,
        reference: &Reference,
        manifest: &Digest,
        media_type: &str,
        size: u64,
        platform: Option<&str>,
    ) -> Result<()> {
        let name = reference.to_string();
        self.with_index(|index| {
            index
                .manifests
                .retain(|entry| entry.annotations.get(REF_NAME) != Some(&name));
            index.manifests.push(Entry {
                media_type: media_type.to_string(),
                digest: manifest.to_string(),
                size,
                annotations: BTreeMap::from([(REF_NAME.to_string(), name.clone())]),
                platform: platform.and_then(|platform| {
                    platform.split_once('/').map(|(os, architecture)| Platform {
                        os: os.to_string(),
                        architecture: architecture.to_string(),
                    })
                }),
            });
            // An order nobody has to diff against last time.
            index.manifests.sort_by(|a, b| {
                a.annotations
                    .get(REF_NAME)
                    .cmp(&b.annotations.get(REF_NAME))
            });
        })
    }

    /// Every reference registered here, and the manifest it names.
    pub fn images(&self) -> Result<Vec<(String, Digest)>> {
        Ok(self
            .index()?
            .manifests
            .into_iter()
            .filter_map(|entry| {
                let name = entry.annotations.get(REF_NAME)?.clone();
                Some((name, Digest::parse(&entry.digest).ok()?))
            })
            .collect())
    }

    fn index_path(&self) -> PathBuf {
        self.root.join("index.json")
    }

    /// The index as it stands, or an empty one where there is no cache yet.
    fn index(&self) -> Result<Index> {
        let path = self.index_path();
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Index::default()),
            Err(e) => return Err(Error::io(format!("reading {}", path.display()), e)),
        };
        serde_json::from_slice(&bytes).map_err(|e| {
            Error::io(
                format!("reading {}", path.display()),
                std::io::Error::other(e),
            )
        })
    }

    /// Read the index, change it, and put it back, with every other process holding off.
    ///
    /// The lock is on a file of its own, because the index itself is replaced by a rename
    /// and a lock on a replaced file guards nothing.
    fn with_index(&self, change: impl FnOnce(&mut Index)) -> Result<()> {
        fs::create_dir_all(&self.root)
            .map_err(|e| Error::io(format!("making {}", self.root.display()), e))?;

        let guard = self.root.join("index.lock");
        let lock = File::create(&guard)
            .map_err(|e| Error::io(format!("opening {}", guard.display()), e))?;
        lock.lock()
            .map_err(|e| Error::io(format!("locking {}", guard.display()), e))?;

        let mut index = self.index()?;
        change(&mut index);

        // What marks the directory as an image layout. Written beside the index, since a
        // layout with no `index.json` is not one anything would read.
        let layout = self.root.join("oci-layout");
        fs::write(&layout, br#"{"imageLayoutVersion": "1.0.0"}"#)
            .map_err(|e| Error::io(format!("writing {}", layout.display()), e))?;

        let bytes = serde_json::to_vec_pretty(&index)
            .map_err(|e| Error::io("writing the index", std::io::Error::other(e)))?;
        let staged = self.staging("index")?;
        fs::write(&staged, &bytes)
            .map_err(|e| Error::io(format!("writing {}", staged.display()), e))?;
        fs::rename(&staged, self.index_path())
            .map_err(|e| Error::io(format!("writing {}", self.index_path().display()), e))?;
        Ok(())
    }
}

/// `index.json`, as far as anything here reads it.
#[derive(Serialize, Deserialize)]
struct Index {
    #[serde(rename = "schemaVersion")]
    schema_version: u8,
    #[serde(rename = "mediaType", skip_serializing_if = "Option::is_none")]
    media_type: Option<String>,
    manifests: Vec<Entry>,
}

impl Default for Index {
    fn default() -> Index {
        Index {
            schema_version: 2,
            media_type: Some("application/vnd.oci.image.index.v1+json".to_string()),
            manifests: Vec::new(),
        }
    }
}

#[derive(Serialize, Deserialize)]
struct Entry {
    #[serde(rename = "mediaType")]
    media_type: String,
    digest: String,
    size: u64,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    annotations: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    platform: Option<Platform>,
}

#[derive(Serialize, Deserialize)]
struct Platform {
    architecture: String,
    os: String,
}

impl Default for Cache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Scratch {
            let path =
                std::env::temp_dir().join(format!("cortex-krun-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            Scratch(path)
        }

        fn cache(&self) -> Cache {
            Cache::at(&self.0)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_blob_comes_back_as_it_went_in() {
        let scratch = Scratch::new("blob");
        let cache = scratch.cache();

        let digest = cache.put(b"layer bytes").unwrap();
        assert!(cache.has(&digest));
        assert_eq!(cache.read(&digest).unwrap(), b"layer bytes");
        assert_eq!(digest, Digest::of(b"layer bytes"));
    }

    #[test]
    fn the_same_bytes_are_one_blob() {
        let scratch = Scratch::new("dedup");
        let cache = scratch.cache();

        let first = cache.put(b"shared").unwrap();
        let again = cache.put(b"shared").unwrap();
        assert_eq!(first, again);

        let blobs = cache.root().join("blobs").join("sha256");
        assert_eq!(fs::read_dir(blobs).unwrap().count(), 1);
    }

    #[test]
    fn staging_does_not_accumulate() {
        let scratch = Scratch::new("tmp");
        let cache = scratch.cache();

        cache.put(b"one").unwrap();
        cache.put(b"two").unwrap();

        let leftover = fs::read_dir(cache.root().join("tmp")).unwrap().count();
        assert_eq!(leftover, 0, "a staged file was not moved or removed");
    }

    const MANIFEST_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";

    fn reference(text: &str) -> Reference {
        text.parse().unwrap()
    }

    #[test]
    fn a_reference_is_remembered_and_read_back() {
        let scratch = Scratch::new("refs");
        let cache = scratch.cache();
        let manifest = Digest::of(b"a manifest");

        assert_eq!(cache.resolved(&reference("alpine:latest")), None);
        cache
            .register_reference(
                &reference("alpine:latest"),
                &manifest,
                MANIFEST_TYPE,
                901,
                Some("linux/arm64"),
            )
            .unwrap();
        assert_eq!(cache.resolved(&reference("alpine:latest")), Some(manifest));
        assert_eq!(cache.resolved(&reference("alpine:3.20")), None);
    }

    /// Two spellings of one image are one memo.
    #[test]
    fn spellings_of_one_reference_agree() {
        let scratch = Scratch::new("canon");
        let cache = scratch.cache();
        let manifest = Digest::of(b"a manifest");

        cache
            .register_reference(
                &reference("alpine:3.21"),
                &manifest,
                MANIFEST_TYPE,
                901,
                Some("linux/arm64"),
            )
            .unwrap();
        for spelling in [
            "alpine:3.21",
            "docker.io/library/alpine:3.21",
            "index.docker.io/library/alpine:3.21",
        ] {
            assert_eq!(
                cache.resolved(&reference(spelling)),
                Some(manifest.clone()),
                "{spelling} did not find the memo"
            );
        }
        assert_eq!(cache.images().unwrap().len(), 1);
    }

    #[test]
    fn remembering_again_replaces() {
        let scratch = Scratch::new("moved");
        let cache = scratch.cache();

        for manifest in [Digest::of(b"before"), Digest::of(b"after")] {
            cache
                .register_reference(
                    &reference("alpine:latest"),
                    &manifest,
                    MANIFEST_TYPE,
                    901,
                    None,
                )
                .unwrap();
        }
        assert_eq!(
            cache.resolved(&reference("alpine:latest")),
            Some(Digest::of(b"after"))
        );
    }
}
