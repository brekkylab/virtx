//! The disk: what this host keeps of the images it has pulled.
//!
//! Knows nothing of registries or of what a layer holds: files under names, and which
//! reference names which image.

use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use oci_spec::distribution::Reference;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::file::write_atomically;
use crate::filesystem::{self, Digest, Erofs, Tree};
use crate::image::{Image, chain_id};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A layer that could not be written as an EROFS image.
    #[error(transparent)]
    Filesystem(#[from] filesystem::Error),

    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },
}

impl Error {
    pub(crate) fn io(context: impl Into<String>, source: std::io::Error) -> Error {
        Error::Io {
            context: context.into(),
            source,
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// How recently a file may have been written and still be spared by a
/// [`prune`](Cache::prune), when the caller has no better answer.
///
/// An hour, which is the shape of the thing being guarded against rather than a measured
/// number: a layer exists before anything names it -- a pull writes every layer and only
/// then the image and the reference -- and the window is one pull of one image, which an
/// hour outlasts over any link worth using.
pub const GRACE: Duration = Duration::from_secs(60 * 60);

/// What a [`remove`](Cache::remove) came to.
#[derive(Clone, Debug, Default)]
pub struct Removed {
    /// Every reference that named the image, all of which are gone.
    pub untagged: Vec<Reference>,
    /// The files deleted: the image, its layers no other named image stacks, and its root
    /// filesystem unless another named image is made of the same layers.
    pub files: Vec<PathBuf>,
    /// What those files occupied, in bytes.
    pub bytes: u64,
}

/// What a [`prune`](Cache::prune) came to.
#[derive(Clone, Debug, Default)]
pub struct Pruned {
    /// The files deleted.
    pub files: Vec<PathBuf>,
    /// What they occupied, in bytes.
    pub bytes: u64,
    /// Files nothing named and that were left anyway, being younger than the grace period.
    /// Reported rather than passed over, because a caller looking at a cache larger than
    /// its references explain is owed the reason.
    pub spared: usize,
}

/// Where to keep images when the caller does not say: `images` under
/// [`virtx::cache_root`], which `VIRTX_HOME` answers for.
///
/// `VIRTX_IMAGE_CACHE_ROOT` still overrides it, being the narrower of the two: somebody
/// keeping images on another disk is saying something about images and not about
/// everything this host keeps.
fn cache_root() -> PathBuf {
    if let Some(named) = std::env::var_os("VIRTX_IMAGE_CACHE_ROOT") {
        return PathBuf::from(named);
    }

    virtx::cache_root().join("images")
}

/// What a file under `ref/` holds: the reference it is, spelled out, and the manifest
/// digest of the image it names.
///
/// The reference is in the file because the file's name is a hash of it and cannot be
/// read back; listing the references here is reading these.
#[derive(Serialize, Deserialize)]
struct Ref {
    reference: String,
    manifest: Digest,
}

/// Local cache for the VM images downloaded to this machine.
///
/// # Layout
///
/// ```text
/// <root>/
///   ref/<hex>.json              one per reference pulled here: which image it names
///   image/sha256/<hex>.json     one per image, named by its manifest digest
///   layer/sha256/<hex>.erofs    one EROFS per layer, named by its diff_id
///   rootfs/sha256/<hex>.erofs   one merged metadata image per chain of layers
///                /<hex>.vmdk    and the disk it fronts, both named by the ChainID
/// ```
///
/// Three shelves, each naming the next: a reference names an image by its manifest
/// digest, and an image names its layers by their `diff_id`s. A fourth, `rootfs/`, is
/// made of the third. Only `ref/` is ever changed; the others are named by what they hold, so
/// a name that exists is a whole file and the file under it is never anything else.
/// Nothing lands on any of them except by a rename once it is whole, so a name that
/// exists is a file that was finished.
///
/// ## `ref/`
/// The only part that goes stale, and must, because a tag is a name somebody else can
/// move. One file per reference, so that nothing has to be locked: a reference is
/// replaced by a rename, a reader sees the old file or the new one and never half of
/// either, and two pulls of different references touch different files. Two pulls of
/// the same reference at once leave whichever finished last, which is what either of
/// them would have left alone.
///
/// Named by the SHA-256 of the reference's canonical spelling, so `alpine:3.21` and
/// `docker.io/library/alpine:3.21` are one file. A hash and not the reference itself as a
/// path, because the reference does not survive being one on every host: a registry's
/// port is a colon, which NTFS refuses, and tags are case-sensitive where APFS and NTFS
/// by default are not -- `Latest` and `latest` would be one file.
///
/// ## `image/`
/// What a boot reads of an image: its layers, in order, and what to run. Named by the
/// manifest digest, so a reference that carries one -- `alpine@sha256:...` -- finds its
/// image here whatever name it was pulled under, without a reference being asked at all.
///
/// ## `layer/`
/// Named by the layer's `diff_id` -- the digest of its archive uncompressed rather than of
/// whatever a registry compressed it into -- which is also what the image config signs,
/// so a pull can ask whether a layer is already here before fetching a byte of it.
///
/// ## `rootfs/`
/// An image's layers made into its root filesystem, as one block device a boot is handed.
/// Named by the layers' ChainID -- the image spec's name for layers applied in order --
/// and not by the image, because the layers are all a root filesystem is made of: two
/// images over the same layers, differing in their config, share one. Made here from
/// `layer/` rather than pulled.
///
/// A `.vmdk` names its extents by absolute path, because a VM reads it from wherever it
/// happens to stand. A cache that is moved is one whose disks have to be made again.
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

    /// Where this layer's EROFS image lies, whether or not anything is there.
    pub fn layer_path(&self, diff_id: &Digest) -> PathBuf {
        self.shelf("layer", diff_id, "erofs")
    }

    /// Whether a download *and* a conversion can both be skipped. Answerable before
    /// anything is fetched, because the config names every layer's `diff_id`.
    pub fn has_layer(&self, diff_id: &Digest) -> bool {
        self.layer_path(diff_id).is_file()
    }

    /// File `tree` as the layer `diff_id` names, written as the EROFS image a root
    /// filesystem is stacked from.
    ///
    /// `diff_id` is the caller's word for what `tree` is -- the digest of its archive
    /// uncompressed, which an image config signs and
    /// [`Changeset`](crate::filesystem::Changeset) answers with on the way in or out -- and
    /// it is not checked here: the caller is the one holding what it was checked against.
    ///
    /// Under the name only once it is whole, so a failure leaves nothing a later
    /// [`has_layer`](Cache::has_layer) would take for a layer. One already there is replaced
    /// by what, the name addressing what it holds, it already was.
    pub fn put_layer(&self, diff_id: &Digest, tree: &Tree) -> Result<()> {
        let path = self.layer_path(diff_id);
        write_atomically(&path, |file| Erofs::from_tree(tree, file))
            .map_err(|e| Error::io(format!("writing {}", path.display()), e))??;
        Ok(())
    }

    /// Where the root filesystem `layers`, bottom first, make is kept, whether or not it
    /// is there: named by the layers' ChainID, its two files beside it (see
    /// [`Rootfs`](crate::rootfs::Rootfs)). `None` for no layers, which make no filesystem.
    pub fn rootfs_path(&self, layers: &[Digest]) -> Option<PathBuf> {
        let chain_id = chain_id(layers)?;
        Some(
            self.root
                .join("rootfs")
                .join(chain_id.algorithm().as_ref())
                .join(chain_id.digest()),
        )
    }

    /// The image whose manifest digest is `manifest`, or nothing if it is not here.
    pub fn get_image(&self, manifest: &Digest) -> Result<Option<Image>> {
        read(&self.shelf("image", manifest, "json"))
    }

    /// File `image` under its manifest digest.
    ///
    /// The same digest is always the same image, so whatever was there is replaced by
    /// what it already was.
    pub fn put_image(&self, image: &Image) -> Result<()> {
        self.write(self.shelf("image", &image.manifest, "json"), image)
    }

    /// The manifest digest `reference` named when it was last pulled here, or nothing if
    /// it never was.
    pub fn get_ref(&self, reference: &Reference) -> Result<Option<Digest>> {
        Ok(read::<Ref>(&self.ref_path(reference))?.map(|read| read.manifest))
    }

    /// Have `reference` name the image whose manifest digest is `manifest`, replacing
    /// whatever it named before.
    ///
    /// Put the image first: a reference written before its image names nothing.
    pub fn set_ref(&self, reference: &Reference, manifest: &Digest) -> Result<()> {
        let written = Ref {
            reference: reference.to_string(),
            manifest: manifest.clone(),
        };
        self.write(self.ref_path(reference), &written)
    }

    /// Every reference pulled here, and the manifest digest it names, in the order their
    /// canonical spellings sort in.
    ///
    /// A file that is there and cannot be read is an error rather than left out, because
    /// answering without it would say a reference is not here that is.
    pub fn refs(&self) -> Result<Vec<(Reference, Digest)>> {
        let dir = self.root.join("ref");
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(Error::io(format!("reading {}", dir.display()), e)),
        };

        let mut refs = Vec::new();
        for entry in entries {
            let path = entry
                .map_err(|e| Error::io(format!("reading {}", dir.display()), e))?
                .path();
            // Gone between the listing and the read: removed, so not here.
            let Some(read) = read::<Ref>(&path)? else {
                continue;
            };
            let reference = read
                .reference
                .parse::<Reference>()
                .map_err(|e| unreadable(&path, e))?;
            refs.push((read.reference, reference, read.manifest));
        }

        refs.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(refs
            .into_iter()
            .map(|(_, reference, manifest)| (reference, manifest))
            .collect())
    }

    /// The image `reference` names here, by the name it was pulled under or by the digest
    /// it carries.
    ///
    /// Two lookups, because an image has two kinds of name and only one of them is the
    /// one it was filed under. A reference finds what it was pulled as, and `alpine:3.21`
    /// finds what `docker.io/library/alpine:3.21` pulled; failing that a manifest digest
    /// names the image itself rather than the way somebody reached it, so
    /// `repo@sha256:...` finds what arrived under a tag -- whatever repository it is
    /// spelled with, the digest being the stronger of the two names.
    pub fn find(&self, reference: &Reference) -> Result<Option<Image>> {
        if let Some(manifest) = self.get_ref(reference)? {
            return self.get_image(&manifest);
        }
        match reference.digest().map(str::parse::<Digest>) {
            Some(Ok(manifest)) => self.get_image(&manifest),
            // A digest that does not parse names no file here.
            Some(Err(_)) | None => Ok(None),
        }
    }

    /// Take the reference `reference` away, and answer with the manifest digest it named,
    /// or nothing if it named nothing here.
    ///
    /// The image stays, as do its other references and every file it is made of: a name is
    /// a key, and taking one away is not taking away what it pointed at. That is
    /// [`remove`](Cache::remove).
    pub fn remove_ref(&self, reference: &Reference) -> Result<Option<Digest>> {
        let Some(manifest) = self.get_ref(reference)? else {
            return Ok(None);
        };
        let path = self.ref_path(reference);
        fs::remove_file(&path).map_err(|e| Error::io(format!("removing {}", path.display()), e))?;
        Ok(Some(manifest))
    }

    /// Take away the image `reference` finds -- every reference to it, not only the one used
    /// to find it -- and then the files only it was holding. `None` if it finds nothing.
    ///
    /// An image is one thing, so removing it is one act; taking one name away and leaving
    /// the rest is [`remove_ref`](Cache::remove_ref). What is deleted then is only what this
    /// image was made of and no image still named here is: its layers, except those another
    /// stacks, and its root filesystem, unless another is made of the same layers. Nothing
    /// else is examined -- a layer a pull in flight has written was never part of this image,
    /// so it is never a candidate.
    ///
    /// The references and the image go before any file does. The two are not one act, so a
    /// process that dies between them leaves files nothing names -- which costs space until
    /// a [`prune`](Cache::prune) and nothing else. The other order would leave names with
    /// nothing behind them.
    ///
    /// Not locked against another process. One that files an image over the same layers
    /// while this is deciding what is still named can lose a layer to it; the next pull of
    /// that image finds the layer missing and fetches it again.
    pub fn remove(&self, reference: &Reference) -> Result<Option<Removed>> {
        let Some(image) = self.find(reference)? else {
            return Ok(None);
        };

        let mut removed = Removed::default();
        for (named, manifest) in self.refs()? {
            if manifest == image.manifest {
                self.remove_ref(&named)?;
                removed.untagged.push(named);
            }
        }
        let filed = self.shelf("image", &image.manifest, "json");
        unlink(&filed, &mut removed.files, &mut removed.bytes);

        let kept = self.named()?;
        let mut candidates: Vec<PathBuf> = image
            .layers
            .iter()
            .map(|layer| self.layer_path(layer))
            .collect();
        if let Some(rootfs) = self.rootfs_path(&image.layers) {
            candidates.extend(files_of(&rootfs));
        }
        for path in candidates {
            if !kept.contains(&stem(&path)) {
                unlink(&path, &mut removed.files, &mut removed.bytes);
            }
        }
        Ok(Some(removed))
    }

    /// Delete everything in the cache no reference reaches, written longer ago than
    /// `grace`.
    ///
    /// What a reference reaches is its image, the image's layers, and its root filesystem;
    /// everything else on `image/`, `layer/` and `rootfs/` -- an image fetched on the way to
    /// a build and never named, what a removal left when it was interrupted, a root
    /// filesystem made of a stack no image here has -- is deleted, as is what a write that
    /// never finished left beside a name. No reference is taken away.
    ///
    /// `grace` spares what was written more recently than that, because a layer exists
    /// before anything names it: a pull writes its layers and only then the image and the
    /// reference, and a sweep in between would delete what it is about to file. [`GRACE`] is
    /// the answer for a sweep running while anything else might be; `Duration::ZERO` sweeps
    /// everything and is only right when nothing else is using this cache.
    pub fn prune(&self, grace: Duration) -> Result<Pruned> {
        let kept = self.named()?;
        let younger = SystemTime::now().checked_sub(grace);
        let mut pruned = Pruned::default();

        for shelf in ["image", "layer", "rootfs", "ref"] {
            for path in files_under(&self.root.join(shelf))? {
                let finished = path.extension().is_some_and(|e| e != "part");
                let named = match shelf {
                    // A reference is what names, and is never itself taken away -- only what
                    // a write of one left unfinished.
                    "ref" => finished,
                    _ => finished && kept.contains(&stem(&path)),
                };
                if named {
                    continue;
                }
                let modified = fs::metadata(&path).and_then(|m| m.modified()).ok();
                if younger.is_none_or(|cutoff| modified.is_none_or(|at| at > cutoff)) {
                    pruned.spared += 1;
                    continue;
                }
                unlink(&path, &mut pruned.files, &mut pruned.bytes);
            }
        }
        Ok(pruned)
    }

    /// What every reference here reaches, as each file's path less its extension: the
    /// images, their layers, and their root filesystems.
    fn named(&self) -> Result<HashSet<PathBuf>> {
        let mut kept = HashSet::new();
        for (_, manifest) in self.refs()? {
            kept.insert(stem(&self.shelf("image", &manifest, "json")));
            let Some(image) = self.get_image(&manifest)? else {
                continue;
            };
            for layer in &image.layers {
                kept.insert(stem(&self.layer_path(layer)));
            }
            if let Some(rootfs) = self.rootfs_path(&image.layers) {
                kept.insert(rootfs);
            }
        }
        Ok(kept)
    }

    /// Where something named by `digest` lies on the shelf `name`.
    ///
    /// A digest's algorithm and encoded halves are a directory and a file name, which
    /// keeps the colon out of the path -- NTFS refuses one. Neither half can hold a
    /// separator or a dot of its own: a [`Digest`] that parsed is `[a-z0-9]` joined by
    /// `+._-`, then `[a-zA-Z0-9=_-]`.
    fn shelf(&self, name: &str, digest: &Digest, extension: &str) -> PathBuf {
        self.root
            .join(name)
            .join(digest.algorithm().as_ref())
            .join(format!("{}.{extension}", digest.digest()))
    }

    /// Where `reference`'s file lies, whether or not anything is there.
    fn ref_path(&self, reference: &Reference) -> PathBuf {
        let hash = Sha256::digest(reference.to_string());
        self.root.join("ref").join(format!("{hash:x}.json"))
    }

    /// Put `value` at `to` as JSON, by a rename.
    fn write(&self, to: PathBuf, value: &impl Serialize) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(value).map_err(|e| unreadable(&to, e))?;
        write_atomically(&to, |file| file.write_all(&bytes))
            .and_then(|written| written)
            .map_err(|e| Error::io(format!("writing {}", to.display()), e))
    }
}

impl Default for Cache {
    fn default() -> Self {
        Self::new()
    }
}

/// `path` less its extension, which is what a shelf names a file by: `<hex>.erofs` and
/// `<hex>.vmdk` are one root filesystem's, and `<hex>.erofs.<pid>.part` is nobody's.
fn stem(path: &Path) -> PathBuf {
    path.with_extension("")
}

/// The files of the root filesystem kept at `at`: its metadata image and its disk.
fn files_of(at: &Path) -> [PathBuf; 2] {
    let (meta, disk) = crate::rootfs::files(at);
    [meta, disk]
}

/// Every file under `dir`, however deep. Nothing, for a shelf nothing was ever put on.
fn files_under(dir: &Path) -> Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(Error::io(format!("reading {}", dir.display()), e)),
    };
    let mut files = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| Error::io(format!("reading {}", dir.display()), e))?;
        let path = entry.path();
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            files.extend(files_under(&path)?);
        } else {
            files.push(path);
        }
    }
    Ok(files)
}

/// Delete `path`, and say so in `files` and `bytes`. One that is already gone, or that will
/// not go, is not a failure of what asked: by the time this runs nothing names it, so what
/// is left behind is space and not a broken image.
fn unlink(path: &Path, files: &mut Vec<PathBuf>, bytes: &mut u64) {
    let Ok(size) = fs::metadata(path).map(|m| m.len()) else {
        return;
    };
    if fs::remove_file(path).is_ok() {
        files.push(path.to_path_buf());
        *bytes += size;
    }
}

/// The JSON at `path`, or nothing if there is no file there.
///
/// A file that is there and is not what it should be is an error and not nothing: it was
/// written whole or not at all, so something other than this put it there.
fn read<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Error::io(format!("reading {}", path.display()), e)),
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|e| unreadable(path, e))
}

fn unreadable(path: &Path, e: impl std::error::Error + Send + Sync + 'static) -> Error {
    Error::io(
        format!("reading {}", path.display()),
        std::io::Error::other(e),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Scratch {
            let path =
                std::env::temp_dir().join(format!("virtx-uvm-lib-{name}-{}", std::process::id()));
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

    fn digest(hex: char) -> Digest {
        format!("sha256:{}", hex.to_string().repeat(64))
            .parse()
            .unwrap()
    }

    fn image(manifest: char) -> Image {
        Image {
            manifest: digest(manifest),
            layers: vec![digest('1'), digest('2')],
            config: None,
            platform: "linux/arm64".to_string(),
        }
    }

    fn reference(text: &str) -> Reference {
        text.parse().unwrap()
    }

    /// An image over `layers`, each written as a file of `size` bytes, and its root
    /// filesystem's two files -- what a pull and a boot leave behind, without their contents.
    fn stocked(cache: &Cache, manifest: char, layers: &[char]) -> Image {
        let image = Image {
            manifest: digest(manifest),
            layers: layers.iter().map(|layer| digest(*layer)).collect(),
            config: None,
            platform: "linux/arm64".to_string(),
        };
        for layer in &image.layers {
            let path = cache.layer_path(layer);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, b"layer").unwrap();
        }
        let rootfs = cache.rootfs_path(&image.layers).unwrap();
        fs::create_dir_all(rootfs.parent().unwrap()).unwrap();
        for file in files_of(&rootfs) {
            fs::write(file, b"rootfs").unwrap();
        }
        cache.put_image(&image).unwrap();
        image
    }

    fn there(cache: &Cache, image: &Image) -> (bool, Vec<bool>, bool) {
        let rootfs = cache.rootfs_path(&image.layers).unwrap();
        (
            cache.get_image(&image.manifest).unwrap().is_some(),
            image.layers.iter().map(|l| cache.has_layer(l)).collect(),
            files_of(&rootfs).iter().all(|f| f.is_file()),
        )
    }

    #[test]
    fn removing_a_reference_leaves_the_image() {
        let scratch = Scratch::new("untag");
        let cache = scratch.cache();
        let image = stocked(&cache, 'a', &['1']);
        cache.set_ref(&reference("one:1"), &image.manifest).unwrap();
        cache.set_ref(&reference("two:1"), &image.manifest).unwrap();

        assert_eq!(
            cache.remove_ref(&reference("one:1")).unwrap(),
            Some(digest('a'))
        );
        assert_eq!(cache.remove_ref(&reference("one:1")).unwrap(), None);
        assert_eq!(cache.refs().unwrap().len(), 1);
        assert_eq!(there(&cache, &image), (true, vec![true], true));
    }

    /// Every name goes, and every file only it held -- and nothing another named image still
    /// stacks.
    #[test]
    fn removing_an_image_takes_what_only_it_held() {
        let scratch = Scratch::new("remove");
        let cache = scratch.cache();
        let gone = stocked(&cache, 'a', &['1', '2']);
        let kept = stocked(&cache, 'b', &['1', '3']);
        // Same layers as `gone`, another config: one root filesystem between them.
        let twin = Image {
            manifest: digest('c'),
            ..gone.clone()
        };
        cache.put_image(&twin).unwrap();
        cache.set_ref(&reference("gone:1"), &gone.manifest).unwrap();
        cache.set_ref(&reference("gone:2"), &gone.manifest).unwrap();
        cache.set_ref(&reference("kept:1"), &kept.manifest).unwrap();
        cache.set_ref(&reference("twin:1"), &twin.manifest).unwrap();

        let removed = cache.remove(&reference("gone:1")).unwrap().unwrap();
        assert_eq!(removed.untagged.len(), 2, "{:?}", removed.untagged);
        assert!(cache.get_image(&gone.manifest).unwrap().is_none());
        assert!(
            cache.has_layer(&digest('1')),
            "a layer another image stacks went"
        );
        assert!(
            cache.has_layer(&digest('2')),
            "a layer the twin stacks went"
        );
        assert_eq!(there(&cache, &twin), (true, vec![true, true], true));
        assert_eq!(there(&cache, &kept), (true, vec![true, true], true));

        let removed = cache.remove(&reference("twin:1")).unwrap().unwrap();
        assert!(!cache.has_layer(&digest('2')));
        assert!(cache.has_layer(&digest('1')));
        assert_eq!(removed.files.len(), 4, "{:?}", removed.files);
        assert_eq!(there(&cache, &kept), (true, vec![true, true], true));

        assert!(cache.remove(&reference("nothing:1")).unwrap().is_none());
    }

    /// What no reference reaches goes; what one does stays; what is too young to tell is
    /// spared.
    #[test]
    fn a_prune_takes_what_nothing_names() {
        let scratch = Scratch::new("prune");
        let cache = scratch.cache();
        let named = stocked(&cache, 'a', &['1', '2']);
        let unnamed = stocked(&cache, 'b', &['1', '3']);
        cache
            .set_ref(&reference("named:1"), &named.manifest)
            .unwrap();
        let part = cache
            .layer_path(&digest('4'))
            .with_extension("erofs.123.part");
        fs::write(&part, b"half").unwrap();

        let young = cache.prune(GRACE).unwrap();
        assert!(young.files.is_empty(), "{:?}", young.files);
        assert!(young.spared > 0);

        let swept = cache.prune(Duration::ZERO).unwrap();
        assert_eq!(there(&cache, &named), (true, vec![true, true], true));
        assert!(cache.get_image(&unnamed.manifest).unwrap().is_none());
        assert!(!cache.has_layer(&digest('3')));
        assert!(!part.exists(), "a part-written file was left");
        assert_eq!(swept.spared, 0);
        assert_eq!(cache.refs().unwrap().len(), 1, "a reference was taken");
    }

    /// File `image` and have `name` name it, as a pull would.
    fn pulled(cache: &Cache, name: &str, image: &Image) {
        cache.put_image(image).unwrap();
        cache.set_ref(&reference(name), &image.manifest).unwrap();
    }

    #[test]
    fn a_layer_put_is_one_the_cache_has() {
        let scratch = Scratch::new("layer");
        let cache = scratch.cache();
        let mut tree = Tree::new();
        tree.insert(
            b"etc/hostname",
            filesystem::Entry::File {
                meta: filesystem::Meta::default(),
                data: filesystem::Data::Buffer(b"here".to_vec()),
            },
        )
        .unwrap();

        assert!(!cache.has_layer(&digest('a')));
        cache.put_layer(&digest('a'), &tree).unwrap();
        assert!(cache.has_layer(&digest('a')));

        let read = Erofs::new(fs::File::open(cache.layer_path(&digest('a'))).unwrap())
            .to_tree()
            .unwrap();
        let paths: Vec<_> = read.walk().into_iter().map(|(path, ..)| path).collect();
        assert_eq!(
            paths,
            [b"".to_vec(), b"etc".to_vec(), b"etc/hostname".to_vec()]
        );
    }

    /// A tree that cannot be written leaves nothing a later pull would take for a layer.
    #[test]
    fn a_layer_that_cannot_be_written_is_not_had() {
        let scratch = Scratch::new("unwritable");
        let cache = scratch.cache();
        let mut tree = Tree::new();
        // Longer than any directory entry can name.
        tree.insert(
            &vec![b'x'; 5000],
            filesystem::Entry::Directory {
                meta: filesystem::Meta::default(),
                opaque: false,
            },
        )
        .unwrap();

        assert!(matches!(
            cache.put_layer(&digest('b'), &tree),
            Err(Error::Filesystem(_))
        ));
        assert!(!cache.has_layer(&digest('b')));
        let dir = cache.layer_path(&digest('b'));
        let left = fs::read_dir(dir.parent().unwrap()).unwrap().count();
        assert_eq!(left, 0, "a part-written layer was left behind");
    }

    /// Named by the layers' ChainID, so the same layers in the same order are one root
    /// filesystem, whatever image they are in.
    #[test]
    fn a_rootfs_is_filed_under_its_chain_id() {
        let cache = Cache::at("root");
        assert!(cache.rootfs_path(&[]).is_none());

        let layers = [digest('1'), digest('2')];
        let chain_id = chain_id(&layers).unwrap();
        assert_eq!(
            cache.rootfs_path(&layers).unwrap(),
            Path::new("root")
                .join("rootfs")
                .join("sha256")
                .join(chain_id.digest())
        );
    }

    #[test]
    fn a_layer_is_filed_under_its_diff_id() {
        let cache = Cache::at("root");
        assert_eq!(
            cache.layer_path(&digest('a')),
            Path::new("root")
                .join("layer")
                .join("sha256")
                .join(format!("{}.erofs", "a".repeat(64)))
        );
    }

    #[test]
    fn an_image_comes_back_as_it_went_in() {
        let scratch = Scratch::new("image");
        let cache = scratch.cache();
        assert!(cache.get_image(&digest('a')).unwrap().is_none());

        cache.put_image(&image('a')).unwrap();
        let read = cache.get_image(&digest('a')).unwrap().unwrap();
        assert_eq!(read.manifest, digest('a'));
        assert_eq!(read.layers, image('a').layers);
    }

    /// Every spelling of one reference is one file -- a `Reference` prints canonically,
    /// which is what makes the name a name -- and the last one written is what it names.
    #[test]
    fn every_spelling_of_a_reference_is_one_ref() {
        let scratch = Scratch::new("ref");
        let cache = scratch.cache();
        assert!(cache.refs().unwrap().is_empty(), "no cache yet");
        assert!(cache.get_ref(&reference("alpine:3.21")).unwrap().is_none());

        for (spelling, manifest) in [
            ("alpine:3.21", '0'),
            ("docker.io/library/alpine:3.21", 'a'),
            ("index.docker.io/library/alpine:3.21", 'b'),
        ] {
            pulled(&cache, spelling, &image(manifest));
        }

        let refs = cache.refs().unwrap();
        assert_eq!(refs.len(), 1, "three spellings made {} files", refs.len());
        assert_eq!(refs[0].0.to_string(), "docker.io/library/alpine:3.21");
        assert_eq!(refs[0].1, digest('b'));
        assert_eq!(
            cache.get_ref(&reference("alpine:3.21")).unwrap(),
            Some(digest('b'))
        );
    }

    /// Tags that differ only in case are two references, and stay two files on a host
    /// whose filesystem does not tell them apart.
    #[test]
    fn tags_that_differ_in_case_are_two_refs() {
        let scratch = Scratch::new("case");
        let cache = scratch.cache();

        pulled(&cache, "alpine:Latest", &image('a'));
        pulled(&cache, "alpine:latest", &image('b'));

        assert_eq!(cache.refs().unwrap().len(), 2);
        assert_eq!(
            cache.get_ref(&reference("alpine:Latest")).unwrap(),
            Some(digest('a'))
        );
    }

    /// What was pulled under a tag is found by its tag, and by its digest under any
    /// repository -- and a digest that is not here finds nothing.
    #[test]
    fn an_image_is_found_by_its_name_or_its_digest() {
        let scratch = Scratch::new("find");
        let cache = scratch.cache();
        pulled(&cache, "alpine:3.21", &image('a'));

        let by_tag = cache.find(&reference("alpine:3.21")).unwrap().unwrap();
        assert_eq!(by_tag.manifest, digest('a'));

        let pinned = format!("ghcr.io/somebody/else@sha256:{}", "a".repeat(64));
        let by_digest = cache.find(&reference(&pinned)).unwrap().unwrap();
        assert_eq!(by_digest.manifest, digest('a'));

        let missing = format!("alpine@sha256:{}", "c".repeat(64));
        assert!(cache.find(&reference(&missing)).unwrap().is_none());
        assert!(cache.find(&reference("alpine:3.22")).unwrap().is_none());
    }
}
