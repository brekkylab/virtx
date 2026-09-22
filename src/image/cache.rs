use std::fs;
use std::path::{Path, PathBuf};

use super::{Digest, Error, Reference, Result};

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
/// ```text
/// <root>/
///   blobs/sha256/<hex>   layers, configs and manifests, as the registry served them
///   refs/<hash of ref>   the manifest digest a reference last resolved to
///   tmp/                 downloads in progress
/// ```
///
/// `blobs/` is content-addressed, so it needs no invalidation and a name that exists is
/// bytes that are whole -- nothing lands there except a rename out of `tmp/`.
///
/// `refs/` is the only part that goes stale, and must: a tag is a name somebody else can
/// move.
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

    /// Keep bytes already in hand -- a manifest, a config. A layer goes through
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
    /// not an answer: only the registry knows what a tag means now.
    pub fn resolved(&self, reference: &Reference) -> Option<Digest> {
        let text = fs::read_to_string(self.reference_path(reference)).ok()?;
        Digest::parse(text.trim()).ok()
    }

    pub fn remember(&self, reference: &Reference, manifest: &Digest) -> Result<()> {
        let path = self.reference_path(reference);
        let parent = path.parent().expect("a reference path has a directory");
        fs::create_dir_all(parent)
            .map_err(|e| Error::io(format!("making {}", parent.display()), e))?;

        let staged = self.staging("ref")?;
        fs::write(&staged, format!("{manifest}\n"))
            .map_err(|e| Error::io(format!("writing {}", staged.display()), e))?;
        fs::rename(&staged, &path)
            .map_err(|e| Error::io(format!("writing {}", path.display()), e))?;
        Ok(())
    }

    /// Keyed by the canonical spelling, so that `alpine:3.21` and
    /// `docker.io/library/alpine:3.21` are one memo rather than two. Hashed, because a
    /// reference carries `/` and `:` and nothing reads these by name.
    fn reference_path(&self, reference: &Reference) -> PathBuf {
        self.root
            .join("refs")
            .join(Digest::of(reference.to_string().as_bytes()).hex())
    }
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
            .remember(&reference("alpine:latest"), &manifest)
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
            .remember(&reference("alpine:3.21"), &manifest)
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
        assert_eq!(fs::read_dir(cache.root().join("refs")).unwrap().count(), 1);
    }

    #[test]
    fn remembering_again_replaces() {
        let scratch = Scratch::new("moved");
        let cache = scratch.cache();

        cache
            .remember(&reference("alpine:latest"), &Digest::of(b"before"))
            .unwrap();
        cache
            .remember(&reference("alpine:latest"), &Digest::of(b"after"))
            .unwrap();
        assert_eq!(
            cache.resolved(&reference("alpine:latest")),
            Some(Digest::of(b"after"))
        );
    }
}
