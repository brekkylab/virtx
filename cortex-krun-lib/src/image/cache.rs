use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{Seek, SeekFrom, Write};
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};

use super::{Digest, Error, Image, Result};

/// The index, under the root.
const INDEX: &str = "images.json";

/// What guards it, beside it.
const LOCK: &str = "images.lock";

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
///   images.json                 one entry per reference pulled here
///   images.lock                 guards `images.json`
///   layer/sha256/<hex>.erofs    one EROFS per layer, named by its digest
///   fsmeta/sha256/<hex>.erofs   one stack's layers merged, metadata alone
///   disk/sha256/<hex>.vmdk      what reads that and those layers as one disk
///   tmp/                        what is being written
/// ```
///
/// ## `images.json`
/// The only part that goes stale, and must, because a tag is a name somebody else can
/// move. Read and written under a lock, being the one file every pull rewrites.
///
/// Keyed by the reference as it is spelled canonically, so `alpine:3.21` and
/// `docker.io/library/alpine:3.21` are one entry. What each holds is what a boot needs
/// and nothing more: which layers, in which order, and what to run.
///
/// ```json
/// {
///   "docker.io/library/alpine:3.21": {
///     "manifest": "sha256:0a7ed0d449b9318548e66674610d757de19b7645759f74b587b610b59d6b43fd",
///     "layers": [
///       { "digest": "sha256:a16e98724c05975ee8c40d8fe389c3481373d34ab20a1cf52ea2accc43f71f4c" }
///     ],
///     "config": { "Env": ["PATH=/usr/sbin:/usr/bin:/sbin:/bin"], "Cmd": ["/bin/sh"] },
///     "platform": "linux/arm64"
///   }
/// }
/// ```
///
/// ## `fsmeta/` and `disk/`
/// The other two content-addressed shelves, named by the *stack* -- the ordered layers
/// of one image -- rather than by one layer. `fsmeta/` holds those layers' trees merged
/// and written as metadata alone, and `disk/` the descriptor that reads it and them as
/// one block device. Neither holds a byte of any layer's contents, so two images over
/// one base cost one copy of the base.
///
/// ## `layer/`
/// Content-addressed, so it needs no invalidation and a name that exists is a whole
/// file. The name is the layer's `diff_id` -- the digest of its contents rather than of
/// whatever a registry compressed them into -- which is also what the image config
/// signs, so a pull can ask whether a layer is already here before fetching a byte of
/// it. Nothing lands there except a rename out of `tmp/`.
///
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

    /// Where this layer's EROFS image lies, whether or not anything is there. Named by
    /// the layer's `digest`, which is what the image config signs and so names the
    /// same contents whatever a registry happened to compress them with.
    pub fn layer_path(&self, digest: &Digest) -> PathBuf {
        let mut path = self.root.join("layer").join(digest.path());
        path.set_extension("erofs");
        path
    }

    /// Whether a download *and* a conversion can both be skipped. Answerable before
    /// anything is fetched, because the config names every layer's `diff_id`.
    pub fn has_layer(&self, digest: &Digest) -> bool {
        self.layer_path(digest).is_file()
    }

    /// Where a stack's merged metadata lies, whether or not anything is there.
    ///
    /// Named by the stack rather than by any of the layers in it: what is in the file
    /// is every one of their trees merged, so it belongs to the ordered set and not to
    /// a member of it.
    pub fn fsmeta_path(&self, stack: &Digest) -> PathBuf {
        let mut path = self.root.join("fsmeta").join(stack.path());
        path.set_extension("erofs");
        path
    }

    /// Where the descriptor that reads a stack as one disk lies.
    ///
    /// A few hundred bytes of text naming the files it is made of, which is why it is
    /// kept rather than made afresh: what it names is where those files are on *this*
    /// host, and a disk whose descriptor is here is one a VM can be handed.
    pub fn disk_path(&self, stack: &Digest) -> PathBuf {
        let mut path = self.root.join("disk").join(stack.path());
        path.set_extension("vmdk");
        path
    }

    /// Whether a stack has already been put together. Both halves, because a
    /// descriptor names its metadata image and a descriptor without one reads a disk
    /// that is not there.
    pub fn has_disk(&self, stack: &Digest) -> bool {
        self.disk_path(stack).is_file() && self.fsmeta_path(stack).is_file()
    }

    /// Somewhere to write `to` before it is `to`.
    ///
    /// Keeps half-finished bytes out of the way of anything looking for a finished
    /// one, and removes them again if they stay half-finished. The destination is
    /// named here rather than at the commit, because every caller knows it before it
    /// writes a byte -- and naming it once is what keeps a file from being staged for
    /// one thing and committed as another.
    pub fn staged_writer(&self, to: impl Into<PathBuf>) -> Result<Staged> {
        let to = to.into();

        let tmp = self.root.join("tmp");
        fs::create_dir_all(&tmp).map_err(|e| Error::io(format!("making {}", tmp.display()), e))?;

        // The destination's own name, so a file left in `tmp/` says what it was going
        // to be, and two of them in one process cannot collide.
        let name = to.file_name().unwrap_or(to.as_os_str()).to_string_lossy();
        let at = tmp.join(format!("{}.{name}.part", std::process::id()));
        let file = fs::File::create(&at)
            .map_err(|e| Error::io(format!("creating {}", at.display()), e))?;

        Ok(Staged {
            file: Some(file),
            at,
            to,
        })
    }

    /// Every reference pulled here, and the image it named.
    ///
    /// The whole map, because it is one file of a few lines an entry: a caller after
    /// one of them is a caller that can look at the rest itself.
    ///
    /// No lock. An update lands by a rename, so what this reads is one whole index or
    /// the one before it, never half of either. What it cannot do is be changed and put
    /// back -- [`image_index_mut`](Self::image_index_mut) is that.
    ///
    /// An index that is not there yet is an empty one. An index that is there and
    /// cannot be read is an error, because answering "no images" would lose every one
    /// of them at the next write.
    pub fn image_index(&self) -> Result<BTreeMap<String, Image>> {
        let path = self.root.join(INDEX);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
            Err(e) => return Err(Error::io(format!("reading {}", path.display()), e)),
        };

        serde_json::from_slice(&bytes).map_err(|e| {
            Error::io(
                format!("reading {}", path.display()),
                std::io::Error::other(e),
            )
        })
    }

    /// The same, to change and put back, with the file locked until it is.
    ///
    /// Everything between this and [`ImageIndex::commit`] is inside the lock, which is
    /// what keeps two pulls at once from leaving only the second one's entry.
    ///
    /// The lock is on a file of its own, because the index is replaced by a rename and
    /// a lock on a replaced file guards nothing. What holds it is the open file rather
    /// than the file's existence, so a process that dies gives the lock up instead of
    /// leaving one nothing can take.
    pub fn image_index_mut(&self) -> Result<ImageIndex> {
        fs::create_dir_all(&self.root)
            .map_err(|e| Error::io(format!("making {}", self.root.display()), e))?;

        let path = self.root.join(LOCK);
        let lock =
            File::create(&path).map_err(|e| Error::io(format!("opening {}", path.display()), e))?;
        lock.lock()
            .map_err(|e| Error::io(format!("locking {}", path.display()), e))?;

        Ok(ImageIndex {
            images: self.image_index()?,
            cache: self.clone(),
            _lock: lock,
        })
    }
}

impl Default for Cache {
    fn default() -> Self {
        Self::new()
    }
}

/// A part-written file, which removes itself when it goes out of scope.
///
/// Written through rather than beside: this *is* the writer, so a caller never holds
/// the staging path and cannot write somewhere else by accident. [`Seek`] as well as
/// [`Write`], because the one thing written this way -- an EROFS image -- is laid out
/// rather than streamed.
///
/// Committing renames the file away, so by the time the removal runs there is nothing
/// left to remove. What it is for is the other endings: a conversion that refused what
/// it was given, and a task that panicked part-way. Neither leaves anything in `tmp/`.
pub struct Staged {
    /// Open until it is committed or dropped. An `Option` because both of those have
    /// to close it before they touch the name -- Windows renames and removes nothing
    /// that is still open.
    file: Option<fs::File>,
    /// Where it is being written.
    at: PathBuf,
    /// Where it belongs once it is whole.
    to: PathBuf,
}

impl Staged {
    /// Where it is being written, for saying so in an error.
    pub fn path(&self) -> &Path {
        &self.at
    }

    /// Move what was written where it belongs, making its directory if it is not there.
    ///
    /// A rename, so nothing is ever half-written under the destination: either the
    /// whole file is there or the name is not. Consuming, because afterwards there is
    /// nothing left to write to.
    ///
    /// Whatever was there is replaced. Where the destination is addressed by what it
    /// holds, that is a file being replaced by the same bytes.
    pub fn commit(mut self) -> Result<()> {
        drop(self.file.take());

        let parent = self.to.parent().expect("a committed path has a directory");
        fs::create_dir_all(parent)
            .map_err(|e| Error::io(format!("making {}", parent.display()), e))?;

        fs::rename(&self.at, &self.to).map_err(|e| {
            Error::io(
                format!("moving {} to {}", self.at.display(), self.to.display()),
                e,
            )
        })
    }

    /// The open file, which is there until a commit takes it.
    fn file(&mut self) -> std::io::Result<&mut fs::File> {
        self.file
            .as_mut()
            .ok_or_else(|| std::io::Error::other("a staged file that was already committed"))
    }
}

impl Write for Staged {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.file()?.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file()?.flush()
    }
}

impl Seek for Staged {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.file()?.seek(pos)
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        drop(self.file.take());
        let _ = fs::remove_file(&self.at);
    }
}

/// Index images, which `images.json` holds.
pub struct ImageIndex {
    images: BTreeMap<String, Image>,
    cache: Cache,
    _lock: File,
}

impl ImageIndex {
    /// Write it back, replacing what was there, and let the lock go.
    ///
    /// Into place by a rename, so a reader sees one whole index or the one before it.
    /// Consuming, because what was read is only worth writing while the lock that was
    /// taken with it is still held -- and dropping one instead gives the lock up and
    /// changes nothing, which is what a pull that failed wants.
    pub fn commit(self) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(&self.images)
            .map_err(|e| Error::io("writing the index", std::io::Error::other(e)))?;

        let mut staged = self.cache.staged_writer(self.cache.root.join(INDEX))?;
        staged
            .write_all(&bytes)
            .map_err(|e| Error::io(format!("writing {}", staged.path().display()), e))?;
        staged.commit()
    }
}

impl Deref for ImageIndex {
    type Target = BTreeMap<String, Image>;

    fn deref(&self) -> &BTreeMap<String, Image> {
        &self.images
    }
}

impl DerefMut for ImageIndex {
    fn deref_mut(&mut self) -> &mut BTreeMap<String, Image> {
        &mut self.images
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
    fn staging_does_not_accumulate() {
        let scratch = Scratch::new("tmp");
        let cache = scratch.cache();

        // Dropped rather than committed: what was half-written goes with it.
        let mut abandoned = cache.staged_writer(cache.root().join("abandoned")).unwrap();
        abandoned.write_all(b"half").unwrap();
        drop(abandoned);

        // Committed: moved under its name, and not left behind as well.
        let kept = cache.root().join("done");
        let mut staged = cache.staged_writer(&kept).unwrap();
        staged.write_all(b"whole").unwrap();
        staged.commit().unwrap();

        assert_eq!(fs::read(&kept).unwrap(), b"whole");
        let leftover = fs::read_dir(cache.root().join("tmp")).unwrap().count();
        assert_eq!(leftover, 0, "a staged file was not moved or removed");
    }

    fn reference(text: &str) -> crate::image::Reference {
        text.parse().unwrap()
    }

    /// An image with nothing in it but the one thing this looks at.
    fn image(manifest: Digest) -> Image {
        Image {
            manifest,
            layers: Vec::new(),
            config: Default::default(),
            platform: "linux/arm64".to_string(),
        }
    }

    /// Two things at once, because they are one round trip: what is written comes back,
    /// and every spelling of one image is one entry -- a `Reference` prints canonically,
    /// which is what makes the key a key.
    #[test]
    fn an_index_comes_back_as_it_went_in() {
        let scratch = Scratch::new("index");
        let cache = scratch.cache();
        assert!(cache.image_index().unwrap().is_empty(), "no cache yet");

        let manifest = Digest::of(b"a manifest");
        let mut index = cache.image_index_mut().unwrap();
        for spelling in [
            "alpine:3.21",
            "docker.io/library/alpine:3.21",
            "index.docker.io/library/alpine:3.21",
        ] {
            index.insert(reference(spelling).to_string(), image(manifest.clone()));
        }
        index.commit().unwrap();

        let read = cache.image_index().unwrap();
        assert_eq!(read.len(), 1, "three spellings made {} entries", read.len());
        assert_eq!(read["docker.io/library/alpine:3.21"].manifest, manifest);
    }
}
