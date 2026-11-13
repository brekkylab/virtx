//! What this host keeps of images: layers, the images stacked from them, and the tags that
//! name images.
//!
//! # Layout
//!
//! ```text
//! <root>/
//!   layer/sha256/<hex>.erofs   one per layer, named by this file's digest
//!   rootfs/sha256/<hex>.erofs  one per list of layers, named by the list's digest: them merged
//!                 <hex>.vmdk   ... and the disk of that image and the layers it points into
//!   image/sha256/<hex>.json    one per image: its OCI manifest and its config, named by the
//!                              manifest's digest
//!                <hex>.lock    ... and what holds it for a machine, shared, or takes it away
//!   tag/<hex>.json             one per reference: which image it names
//!   origin/sha256/<hex>.json   one per registry blob or manifest, named by its digest: which layer
//!                              or image it became
//!   lock                       what a change holds while it checks and writes
//! ```
//!
//! An image is its manifest and its config, each as a registry serves it, byte for byte: each
//! layer an EROFS image here, of type [`EROFS_LAYER`] -- uncompressed, so its digest is its
//! diff_id too. Two manifests are two images, however little tells them apart.
//!
//! Each shelf names only what is on the one below it: an image is refused unless its layers
//! are here, a tag unless its image is, and nothing is removed while something
//! still names it. Nothing lands under a name except by a rename once it is whole.
//!
//! An origin and a root filesystem are the exceptions: neither holds what it names, and one
//! whose layer, image or layers are not all here is read as not there. A layer or an image
//! put back is the same file it was, so what was made of it is whole again.
//!
//! An image a machine boots from is held (see [`Store::hold_image`]): a prune leaves it, with
//! its layers and its root filesystem, and removing it is refused.
//!
//! Each change to a layer, an image or a tag -- one filed, renewed or removed, a prune --
//! holds `<root>/lock` from its check to its write, across threads and processes: to every
//! other change the two are one, and each waits for the one under way, so each is blocking
//! work. A root filesystem or an origin is written and removed without it, being read as not
//! there once what it names is gone. Reads are not locked, and read what the last change left.
//!
//! A prune leaves what is younger than its grace, for the pull or build still putting an image
//! together over it: so a layer or an image filed again, or renewed by one that finds it here
//! (see [`Store::renew_layer`]), is young again.

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, BufReader, Write};
use std::iter;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use oci_spec::distribution::Reference;
use oci_spec::image::{
    Descriptor, Digest, ImageConfiguration, ImageManifest, ImageManifestBuilder, MediaType,
    Sha256Digest,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use super::vmdk;
use crate::fstree::{self, FsTree};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0} is not in the store")]
    Missing(Digest),

    #[error("{0} is still named by {1}")]
    InUse(Digest, String),

    /// A manifest that is not one of an image here.
    #[error("not an image's manifest: {0}")]
    Malformed(String),

    #[error(transparent)]
    Fstree(#[from] fstree::Error),

    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: io::Error,
    },
}

impl Error {
    fn io(context: impl Into<String>, source: io::Error) -> Error {
        Error::Io {
            context: context.into(),
            source,
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// The media type of a layer that is an EROFS image, as containerd names it.
pub const EROFS_LAYER: &str = "application/vnd.erofs.layer.v1";

/// An image's manifest and config, as they were filed: a push of anything else would put them
/// under other digests.
#[derive(Serialize, Deserialize)]
struct ImageFile {
    manifest: String,
    config: String,
}

#[derive(Serialize, Deserialize)]
struct TagFile {
    reference: String,
    image: Digest,
}

/// A blob is told from a manifest by its fields: `layer` for the one, `image` for the other.
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum OriginFile {
    Layer {
        layer: Digest,
        diff_id: Digest,
        descriptor: Box<Descriptor>,
    },
    /// The manifest and its config as the registry served them, byte for byte: a push of
    /// anything else would put them under other digests.
    Image {
        image: Digest,
        manifest: String,
        config: String,
    },
}

/// An image held for as long as this lives: see [`Store::hold_image`].
pub struct ImageHold(#[allow(dead_code)] File);

/// The store's changes, one at a time, for as long as this lives: see the module.
struct Change(#[allow(dead_code)] File);

/// What a [`prune`](Store::prune) took away.
#[derive(Debug, Default)]
pub struct Pruned {
    pub images: Vec<Digest>,
    /// The bytes of every file removed.
    pub reclaimed: u64,
}

#[derive(Clone, Debug)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn new(root: impl Into<PathBuf>) -> Result<Store> {
        let root = root.into();
        for dir in [
            root.join("layer").join("sha256"),
            root.join("rootfs").join("sha256"),
            root.join("image").join("sha256"),
            root.join("tag"),
            root.join("origin").join("sha256"),
        ] {
            fs::create_dir_all(&dir)
                .map_err(|e| Error::io(format!("making {}", dir.display()), e))?;
        }
        Ok(Store { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Wait for every change under way, and keep others waiting for as long as the answer lives.
    fn change(&self) -> Result<Change> {
        let path = self.root.join("lock");
        let file =
            File::create(&path).map_err(|e| Error::io(format!("opening {}", path.display()), e))?;
        file.lock()
            .map_err(|e| Error::io(format!("locking {}", path.display()), e))?;
        Ok(Change(file))
    }

    pub fn layer_path(&self, layer: &Digest) -> PathBuf {
        self.shelf("layer", layer, "erofs")
    }

    pub fn contains_layer(&self, layer: &Digest) -> bool {
        self.layer_path(layer).is_file()
    }

    /// File `tree` as a layer under the digest of its EROFS image, which is returned. A layer
    /// already here is renewed.
    ///
    /// The name is only known once the image is written, so it is written beside the layers
    /// first, and named after.
    pub fn insert_layer(&self, tree: &FsTree) -> Result<Digest> {
        let dir = self.root.join("layer").join("sha256");
        let part = write_part(&dir.join("layer"), |file| Ok(tree.to_erofs(file)?))?;
        let named = (|| -> Result<Digest> {
            let mut hasher = Sha256::new();
            let mut file = File::open(&part)
                .map_err(|e| Error::io(format!("reading {}", part.display()), e))?;
            io::copy(&mut file, &mut hasher)
                .map_err(|e| Error::io(format!("reading {}", part.display()), e))?;
            drop(file);
            let digest: Digest = format!("{:x}", hasher.finalize())
                .parse::<Sha256Digest>()
                .expect("sha2 writes 64 lowercase hex characters")
                .into();
            let path = self.layer_path(&digest);
            let _change = self.change()?;
            if renew(&path).map_err(|e| Error::io(format!("renewing {}", path.display()), e))? {
                fs::remove_file(&part)
                    .map_err(|e| Error::io(format!("removing {}", part.display()), e))?;
            } else {
                fs::rename(&part, &path)
                    .map_err(|e| Error::io(format!("moving {} into place", path.display()), e))?;
            }
            Ok(digest)
        })();
        if named.is_err() {
            let _ = fs::remove_file(&part);
        }
        named
    }

    /// File the EROFS image at `from` as layer `layer`: a hard link to it, where `from` is on
    /// the same filesystem, and a copy where it is not. Linked, one file serves every store
    /// that links it, and a store's removal of the layer removes its own link alone. `from` must
    /// be the EROFS image `layer` names -- one another store filed -- which is not read again
    /// here. A layer already here is renewed.
    pub fn link_layer(&self, from: &Path, layer: &Digest) -> Result<()> {
        static LINKS: AtomicU64 = AtomicU64::new(0);
        let path = self.layer_path(layer);
        let _change = self.change()?;
        if renew(&path).map_err(|e| Error::io(format!("renewing {}", path.display()), e))? {
            return Ok(());
        }
        let part = path.with_extension(format!(
            "erofs.{}.{}.link",
            std::process::id(),
            LINKS.fetch_add(1, Ordering::Relaxed)
        ));
        if fs::hard_link(from, &part).is_err() {
            fs::copy(from, &part)
                .map_err(|e| Error::io(format!("copying {}", from.display()), e))?;
        }
        fs::rename(&part, &path).map_err(|e| {
            let _ = fs::remove_file(&part);
            Error::io(format!("moving {} into place", path.display()), e)
        })
    }

    /// Make `layer` young again, if it is here: a prune's grace spares it for the image being
    /// made over it. `false` if it is not here.
    pub fn renew_layer(&self, layer: &Digest) -> Result<bool> {
        let _change = self.change()?;
        let path = self.layer_path(layer);
        renew(&path).map_err(|e| Error::io(format!("renewing {}", path.display()), e))
    }

    pub fn get_layer(&self, layer: &Digest) -> Result<Option<FsTree>> {
        let path = self.layer_path(layer);
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(Error::io(format!("opening {}", path.display()), e)),
        };
        Ok(Some(FsTree::from_erofs(BufReader::new(file))?))
    }

    /// Delete a layer no image stacks. `false` if it was not here.
    pub fn remove_layer(&self, layer: &Digest) -> Result<bool> {
        let _change = self.change()?;
        for image in self.images()? {
            if let Some((_, layers)) = self.get_image(&image)?
                && layers.contains(layer)
            {
                return Err(Error::InUse(layer.clone(), image.to_string()));
            }
        }
        let path = self.layer_path(layer);
        match fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(Error::io(format!("removing {}", path.display()), e)),
        }
    }

    /// Make the root filesystem of `layers`, bottom first, all of which must be here, and
    /// answer with its disk: a VMDK descriptor reading the layers merged into one metadata
    /// image, and then the layers it points into, as one block device. One already made is
    /// left as it is.
    ///
    /// The disk names the layers' files where they are, so it is good for as long as they
    /// stay. Reads every layer's metadata and writes the merged image's, so it is blocking
    /// work.
    pub fn insert_rootfs(&self, layers: &[Digest]) -> Result<PathBuf> {
        if let Some(missing) = layers.iter().find(|layer| !self.contains_layer(layer)) {
            return Err(Error::Missing(missing.clone()));
        }
        let (meta, disk) = self.rootfs_paths(layers);
        if meta.is_file() && disk.is_file() {
            return Ok(disk);
        }

        let mut tree = FsTree::new();
        for layer in layers {
            tree.stack(FsTree::from_erofs_meta(&self.layer_path(layer))?);
        }
        // The images behind the merged one, in the order it addressed them -- which is the
        // disk's, and so the descriptor's.
        let mut behind = Vec::new();
        write_atomically(&meta, |file| {
            behind = tree.to_erofs_meta(file)?;
            Ok(())
        })?;

        // The descriptor last, so that one is never there before the image it fronts.
        let extents: Vec<&Path> = iter::once(meta.as_path())
            .chain(behind.iter().map(|layer| &**layer))
            .collect();
        let text = vmdk::descriptor(&extents)
            .map_err(|e| Error::io(format!("describing {}", disk.display()), e))?;
        write_atomically(&disk, |file| {
            file.write_all(text.as_bytes())
                .map_err(|e| Error::io(format!("writing {}", disk.display()), e))
        })?;
        Ok(disk)
    }

    /// The disk of the root filesystem of `layers`, if it was made and its layers are all
    /// still here.
    pub fn get_rootfs(&self, layers: &[Digest]) -> Option<PathBuf> {
        let (meta, disk) = self.rootfs_paths(layers);
        (meta.is_file() && disk.is_file() && layers.iter().all(|l| self.contains_layer(l)))
            .then_some(disk)
    }

    /// Delete the root filesystem of `layers`. Its layers stay. `false` if it was not here.
    pub fn remove_rootfs(&self, layers: &[Digest]) -> Result<bool> {
        let (meta, disk) = self.rootfs_paths(layers);
        // The disk first, being what says the two are whole.
        let mut removed = false;
        for path in [disk, meta] {
            match fs::remove_file(&path) {
                Ok(()) => removed = true,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(Error::io(format!("removing {}", path.display()), e)),
            }
        }
        Ok(removed)
    }

    /// File an image of `layers`, bottom first, all of which must be here, and `config`, whose
    /// diff_ids are made to be the layers. Its digest -- its manifest's -- is returned. The
    /// same config and layers are the same image.
    pub fn insert_image(&self, config: &ImageConfiguration, layers: &[Digest]) -> Result<Digest> {
        let mut config = config.clone();
        config
            .rootfs_mut()
            .set_diff_ids(layers.iter().map(Digest::to_string).collect());
        let config_body =
            serde_json::to_vec(&config).map_err(|e| Error::io("serializing a config", e.into()))?;
        let config_digest: Digest = format!("{:x}", Sha256::digest(&config_body))
            .parse::<Sha256Digest>()
            .expect("sha2 writes 64 lowercase hex characters")
            .into();

        let mut descriptors = Vec::new();
        for layer in layers {
            let path = self.layer_path(layer);
            let size = match fs::metadata(&path) {
                Ok(metadata) => metadata.len(),
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    return Err(Error::Missing(layer.clone()));
                }
                Err(e) => return Err(Error::io(format!("reading {}", path.display()), e)),
            };
            descriptors.push(Descriptor::new(
                MediaType::Other(EROFS_LAYER.into()),
                size,
                layer.clone(),
            ));
        }
        let manifest = ImageManifestBuilder::default()
            .schema_version(2u32)
            .media_type(MediaType::ImageManifest)
            .config(Descriptor::new(
                MediaType::ImageConfig,
                config_body.len() as u64,
                config_digest,
            ))
            .layers(descriptors)
            .build()
            .expect("every field a manifest needs is set");
        let body = serde_json::to_vec(&manifest)
            .map_err(|e| Error::io("serializing a manifest", e.into()))?;
        self.insert_image_manifest(&body, &config_body)
    }

    /// File the image `manifest` and `config` are, each as it is, and answer with its digest:
    /// the manifest's. Refused unless `config` is the one the manifest names, each layer is an
    /// EROFS one here, and the config's diff_ids are the layers. An image already here is
    /// renewed.
    pub fn insert_image_manifest(&self, manifest: &[u8], config: &[u8]) -> Result<Digest> {
        let parsed: ImageManifest =
            serde_json::from_slice(manifest).map_err(|e| Error::Malformed(e.to_string()))?;
        if format!("{:x}", Sha256::digest(config)) != parsed.config().digest().digest() {
            return Err(Error::Malformed(format!(
                "its config is not {}",
                parsed.config().digest()
            )));
        }
        let _change = self.change()?;
        for layer in parsed.layers() {
            if layer.media_type().to_string() != EROFS_LAYER {
                return Err(Error::Malformed(format!(
                    "layer {} is {}, not {EROFS_LAYER}",
                    layer.digest(),
                    layer.media_type()
                )));
            }
            if !self.contains_layer(layer.digest()) {
                return Err(Error::Missing(layer.digest().clone()));
            }
        }
        let read: ImageConfiguration = serde_json::from_slice(config)
            .map_err(|e| Error::Malformed(format!("its config: {e}")))?;
        let layers: Vec<String> = parsed
            .layers()
            .iter()
            .map(|layer| layer.digest().to_string())
            .collect();
        if *read.rootfs().diff_ids() != layers {
            return Err(Error::Malformed(
                "its config's diff_ids are not its layers".into(),
            ));
        }

        let text = |bytes: &[u8]| {
            String::from_utf8(bytes.to_vec()).map_err(|e| Error::Malformed(e.to_string()))
        };
        let bytes = serde_json::to_vec(&ImageFile {
            manifest: text(manifest)?,
            config: text(config)?,
        })
        .map_err(|e| Error::io("serializing an image", e.into()))?;
        let digest: Digest = format!("{:x}", Sha256::digest(manifest))
            .parse::<Sha256Digest>()
            .expect("sha2 writes 64 lowercase hex characters")
            .into();
        let path = self.shelf("image", &digest, "json");
        if !renew(&path).map_err(|e| Error::io(format!("renewing {}", path.display()), e))? {
            write_atomically(&path, |file| {
                file.write_all(&bytes)
                    .map_err(|e| Error::io(format!("writing {}", path.display()), e))
            })?;
        }
        Ok(digest)
    }

    /// Make `image` young again, if it is here: a prune's grace spares it for the tag about to
    /// name it. `false` if it is not here.
    pub fn renew_image(&self, image: &Digest) -> Result<bool> {
        let _change = self.change()?;
        let path = self.shelf("image", image, "json");
        renew(&path).map_err(|e| Error::io(format!("renewing {}", path.display()), e))
    }

    /// An image's manifest and config, as they were filed.
    pub fn get_image_manifest(&self, image: &Digest) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        Ok(
            read_filed::<ImageFile>(&self.shelf("image", image, "json"))?
                .map(|read| (read.manifest.into_bytes(), read.config.into_bytes())),
        )
    }

    /// An image's config and its layers, bottom first.
    pub fn get_image(&self, image: &Digest) -> Result<Option<(ImageConfiguration, Vec<Digest>)>> {
        let Some((manifest, config)) = self.get_image_manifest(image)? else {
            return Ok(None);
        };
        let manifest: ImageManifest =
            serde_json::from_slice(&manifest).map_err(|e| Error::Malformed(e.to_string()))?;
        let config = serde_json::from_slice(&config)
            .map_err(|e| Error::Malformed(format!("its config: {e}")))?;
        let layers = manifest
            .layers()
            .iter()
            .map(|layer| layer.digest().clone())
            .collect();
        Ok(Some((config, layers)))
    }

    /// Delete an image no tag names and nothing holds. Its layers stay. `false` if it was not
    /// here.
    pub fn remove_image(&self, image: &Digest) -> Result<bool> {
        let _change = self.change()?;
        if let Some((reference, _)) = self.tags()?.into_iter().find(|(_, named)| named == image) {
            return Err(Error::InUse(image.clone(), reference.to_string()));
        }
        self.remove_image_untagged(image)
    }

    /// [`remove_image`](Store::remove_image) by a caller that holds the store's [`Change`] and
    /// has found no tag names `image`.
    fn remove_image_untagged(&self, image: &Digest) -> Result<bool> {
        let path = self.shelf("image", image, "json");
        if !path.is_file() {
            return Ok(false);
        }
        let lock = self.shelf("image", image, "lock");
        let held =
            File::create(&lock).map_err(|e| Error::io(format!("opening {}", lock.display()), e))?;
        match held.try_lock() {
            Ok(()) => {}
            Err(fs::TryLockError::WouldBlock) => {
                return Err(Error::InUse(image.clone(), "a machine".into()));
            }
            Err(fs::TryLockError::Error(e)) => {
                return Err(Error::io(format!("locking {}", lock.display()), e));
            }
        }
        let removed = match fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(Error::io(format!("removing {}", path.display()), e)),
        };
        drop(held);
        // A hold waiting on it finds the image gone once it has the lock.
        let _ = fs::remove_file(&lock);
        removed
    }

    /// Whether a machine holds `image` now ([`Store::hold_image`]): removing it would be
    /// refused.
    pub fn image_held(&self, image: &Digest) -> Result<bool> {
        let lock = self.shelf("image", image, "lock");
        let held = match File::open(&lock) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(Error::io(format!("opening {}", lock.display()), e)),
        };
        match held.try_lock() {
            Ok(()) => Ok(false),
            Err(fs::TryLockError::WouldBlock) => Ok(true),
            Err(fs::TryLockError::Error(e)) => {
                Err(Error::io(format!("locking {}", lock.display()), e))
            }
        }
    }

    /// Hold `image` for a machine, for as long as the answer lives: a prune leaves it, and
    /// removing it is refused. Waits while it is being taken away, and is then refused.
    pub fn hold_image(&self, image: &Digest) -> Result<ImageHold> {
        let path = self.shelf("image", image, "json");
        if !path.is_file() {
            return Err(Error::Missing(image.clone()));
        }
        let lock = self.shelf("image", image, "lock");
        let held =
            File::create(&lock).map_err(|e| Error::io(format!("opening {}", lock.display()), e))?;
        held.lock_shared()
            .map_err(|e| Error::io(format!("locking {}", lock.display()), e))?;
        if !path.is_file() {
            return Err(Error::Missing(image.clone()));
        }
        Ok(ImageHold(held))
    }

    /// Delete what nothing here needs and is older than `grace`: each image no tag names and
    /// nothing holds, then each layer and root filesystem no image left stacks, each origin
    /// whose layer or image is gone, and what a write that stopped part-way left. `grace`
    /// spares what a pull or a build is still putting together. Blocking work.
    pub fn prune(&self, grace: Duration) -> Result<Pruned> {
        let _change = self.change()?;
        let mut pruned = Pruned::default();
        let old = |path: &Path| {
            fs::metadata(path)
                .and_then(|metadata| metadata.modified())
                .is_ok_and(|modified| modified.elapsed().is_ok_and(|age| age >= grace))
        };

        let tagged: HashSet<Digest> = self.tags()?.into_iter().map(|(_, image)| image).collect();
        let mut kept = Vec::new();
        for image in self.images()? {
            let path = self.shelf("image", &image, "json");
            if tagged.contains(&image) || !old(&path) {
                kept.push(image);
                continue;
            }
            let len = fs::metadata(&path).map_or(0, |metadata| metadata.len());
            match self.remove_image_untagged(&image) {
                Ok(true) => {
                    pruned.reclaimed += len;
                    pruned.images.push(image);
                }
                Ok(false) => {}
                Err(Error::InUse(..)) => kept.push(image),
                Err(e) => return Err(e),
            }
        }

        let mut layers = HashSet::new();
        let mut rootfs = HashSet::new();
        for image in &kept {
            if let Some((_, stacked)) = self.get_image(image)? {
                let (meta, _) = self.rootfs_paths(&stacked);
                rootfs.insert(meta.file_stem().unwrap_or_default().to_owned());
                layers.extend(stacked.into_iter().map(|layer| layer.digest().to_owned()));
            }
        }

        // Each shelf's files that are not needed go once they are old -- but an origin
        // whose layer or image is gone is dead whatever its age.
        for shelf in ["layer", "rootfs", "image", "origin", "tag"] {
            let dir = match shelf {
                "tag" => self.root.join(shelf),
                _ => self.root.join(shelf).join("sha256"),
            };
            for entry in fs::read_dir(&dir)
                .map_err(|e| Error::io(format!("reading {}", dir.display()), e))?
            {
                let path = entry
                    .map_err(|e| Error::io(format!("reading {}", dir.display()), e))?
                    .path();
                let stem = path.file_stem().unwrap_or_default();
                let extension = path.extension().unwrap_or_default();
                let needed = match shelf {
                    "layer" => extension == "erofs" && layers.contains(&*stem.to_string_lossy()),
                    "rootfs" => {
                        (extension == "erofs" || extension == "vmdk") && rootfs.contains(stem)
                    }
                    // A lock is needed while its image is there.
                    "image" => extension == "json" || path.with_extension("json").is_file(),
                    "origin" => extension == "json" && self.origin_is_whole(&path),
                    _ => extension == "json",
                };
                let dead = shelf == "origin" && extension == "json";
                if needed || (!dead && !old(&path)) {
                    continue;
                }
                let len = fs::metadata(&path).map_or(0, |metadata| metadata.len());
                match fs::remove_file(&path) {
                    Ok(()) => pruned.reclaimed += len,
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(e) => return Err(Error::io(format!("removing {}", path.display()), e)),
                }
            }
        }
        Ok(pruned)
    }

    /// Every image here, tagged or not.
    pub fn images(&self) -> Result<Vec<Digest>> {
        let dir = self.root.join("image").join("sha256");
        let mut images = Vec::new();
        for entry in
            fs::read_dir(&dir).map_err(|e| Error::io(format!("reading {}", dir.display()), e))?
        {
            let path = entry
                .map_err(|e| Error::io(format!("reading {}", dir.display()), e))?
                .path();
            if path.extension().is_none_or(|e| e != "json") {
                continue;
            }
            let stem = path.file_stem().unwrap_or_default().to_string_lossy();
            let digest = format!("sha256:{stem}").parse().map_err(|e| {
                Error::io(
                    format!("reading {}", path.display()),
                    io::Error::new(io::ErrorKind::InvalidData, e),
                )
            })?;
            images.push(digest);
        }
        Ok(images)
    }

    /// Have `reference` name `image`, which must be here, in place of whatever it named.
    pub fn insert_tag(&self, reference: &Reference, image: &Digest) -> Result<()> {
        let _change = self.change()?;
        if !self.shelf("image", image, "json").is_file() {
            return Err(Error::Missing(image.clone()));
        }
        let path = self.tag_path(reference);
        let bytes = serde_json::to_vec(&TagFile {
            reference: reference.to_string(),
            image: image.clone(),
        })
        .map_err(|e| Error::io("serializing a tag", e.into()))?;
        write_atomically(&path, |file| {
            file.write_all(&bytes)
                .map_err(|e| Error::io(format!("writing {}", path.display()), e))
        })
    }

    pub fn get_tag(&self, reference: &Reference) -> Result<Option<Digest>> {
        Ok(read_filed::<TagFile>(&self.tag_path(reference))?.map(|read| read.image))
    }

    /// Take `reference` away, and say what it named.
    pub fn remove_tag(&self, reference: &Reference) -> Result<Option<Digest>> {
        let _change = self.change()?;
        let Some(image) = self.get_tag(reference)? else {
            return Ok(None);
        };
        let path = self.tag_path(reference);
        match fs::remove_file(&path) {
            Ok(()) => Ok(Some(image)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(Error::io(format!("removing {}", path.display()), e)),
        }
    }

    /// Every tag here and the image it names, ordered by reference.
    pub fn tags(&self) -> Result<Vec<(Reference, Digest)>> {
        let dir = self.root.join("tag");
        let mut tags = Vec::new();
        for entry in
            fs::read_dir(&dir).map_err(|e| Error::io(format!("reading {}", dir.display()), e))?
        {
            let path = entry
                .map_err(|e| Error::io(format!("reading {}", dir.display()), e))?
                .path();
            if path.extension().is_none_or(|e| e != "json") {
                continue;
            }
            let Some(read) = read_filed::<TagFile>(&path)? else {
                continue;
            };
            let reference = read.reference.parse::<Reference>().map_err(|e| {
                Error::io(
                    format!("reading {}", path.display()),
                    io::Error::new(io::ErrorKind::InvalidData, e),
                )
            })?;
            tags.push((reference, read.image));
        }
        tags.sort_by_key(|(reference, _)| reference.to_string());
        Ok(tags)
    }

    /// Record that the registry blob `descriptor` names became `layer`, which must be here.
    /// A pull that finds the blob skips the download.
    ///
    /// Named by the blob's digest, which a pull has checked the bytes against, and not by
    /// `diff_id`, which is only what the image's config says: an origin a config could name
    /// would let one image's layer stand in for another's. `diff_id` is kept for a push, to
    /// say again what the config said.
    pub fn insert_layer_origin(
        &self,
        descriptor: &Descriptor,
        diff_id: &Digest,
        layer: &Digest,
    ) -> Result<()> {
        if !self.contains_layer(layer) {
            return Err(Error::Missing(layer.clone()));
        }
        let path = self.shelf("origin", descriptor.digest(), "json");
        let bytes = serde_json::to_vec(&OriginFile::Layer {
            layer: layer.clone(),
            diff_id: diff_id.clone(),
            descriptor: Box::new(descriptor.clone()),
        })
        .map_err(|e| Error::io("serializing an origin", e.into()))?;
        write_atomically(&path, |file| {
            file.write_all(&bytes)
                .map_err(|e| Error::io(format!("writing {}", path.display()), e))
        })
    }

    /// The layer the registry blob `blob` became, if it is still here.
    pub fn get_layer_origin(&self, blob: &Digest) -> Result<Option<Digest>> {
        let Some(read) = read_filed::<OriginFile>(&self.shelf("origin", blob, "json"))? else {
            return Ok(None);
        };
        match read {
            OriginFile::Layer { layer, .. } => Ok(self.contains_layer(&layer).then_some(layer)),
            OriginFile::Image { .. } => Ok(None),
        }
    }

    /// Every registry blob that became `layer`, with the diff_id its config named it by:
    /// what a push can name in place of uploading `layer` again.
    pub fn layer_origins_of(&self, layer: &Digest) -> Result<Vec<(Descriptor, Digest)>> {
        let dir = self.root.join("origin").join("sha256");
        let mut origins = Vec::new();
        for entry in
            fs::read_dir(&dir).map_err(|e| Error::io(format!("reading {}", dir.display()), e))?
        {
            let path = entry
                .map_err(|e| Error::io(format!("reading {}", dir.display()), e))?
                .path();
            if path.extension().is_none_or(|e| e != "json") {
                continue;
            }
            let Some(read) = read_filed::<OriginFile>(&path)? else {
                continue;
            };
            if let OriginFile::Layer {
                layer: of,
                diff_id,
                descriptor,
            } = read
                && of == *layer
            {
                origins.push((*descriptor, diff_id));
            }
        }
        Ok(origins)
    }

    /// Record that the registry manifest `manifest`, naming the config `config`, became
    /// `image`, which must be here. A pull that finds `digest` skips the config and the
    /// layers.
    ///
    /// Named by `digest`, which a pull has checked: `manifest`'s own, or that of the index
    /// it was chosen from.
    pub fn insert_image_origin(
        &self,
        digest: &Digest,
        manifest: &[u8],
        config: &[u8],
        image: &Digest,
    ) -> Result<()> {
        if !self.shelf("image", image, "json").is_file() {
            return Err(Error::Missing(image.clone()));
        }
        let text = |bytes: &[u8]| {
            String::from_utf8(bytes.to_vec()).map_err(|e| {
                Error::io(
                    format!("recording {digest}"),
                    io::Error::new(io::ErrorKind::InvalidData, e),
                )
            })
        };
        let path = self.shelf("origin", digest, "json");
        let bytes = serde_json::to_vec(&OriginFile::Image {
            image: image.clone(),
            manifest: text(manifest)?,
            config: text(config)?,
        })
        .map_err(|e| Error::io("serializing an origin", e.into()))?;
        write_atomically(&path, |file| {
            file.write_all(&bytes)
                .map_err(|e| Error::io(format!("writing {}", path.display()), e))
        })
    }

    /// The image the registry manifest or index `digest` became, if it is still here.
    pub fn get_image_origin(&self, digest: &Digest) -> Result<Option<Digest>> {
        let Some(read) = read_filed::<OriginFile>(&self.shelf("origin", digest, "json"))? else {
            return Ok(None);
        };
        match read {
            OriginFile::Image { image, .. } => Ok(self
                .shelf("image", &image, "json")
                .is_file()
                .then_some(image)),
            OriginFile::Layer { .. } => Ok(None),
        }
    }

    /// Every registry manifest that became `image`, and the config it names, each as it was
    /// served or pushed: what a push can put back under the same digests.
    pub fn image_origins_of(&self, image: &Digest) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let dir = self.root.join("origin").join("sha256");
        let mut origins = Vec::new();
        for entry in
            fs::read_dir(&dir).map_err(|e| Error::io(format!("reading {}", dir.display()), e))?
        {
            let path = entry
                .map_err(|e| Error::io(format!("reading {}", dir.display()), e))?
                .path();
            if path.extension().is_none_or(|e| e != "json") {
                continue;
            }
            let Some(read) = read_filed::<OriginFile>(&path)? else {
                continue;
            };
            if let OriginFile::Image {
                image: of,
                manifest,
                config,
            } = read
                && of == *image
            {
                origins.push((manifest.into_bytes(), config.into_bytes()));
            }
        }
        Ok(origins)
    }

    /// Whether the origin at `path` still names a layer or an image here.
    fn origin_is_whole(&self, path: &Path) -> bool {
        let Ok(bytes) = fs::read(path) else {
            return false;
        };
        match serde_json::from_slice::<OriginFile>(&bytes) {
            Ok(OriginFile::Layer { layer, .. }) => self.contains_layer(&layer),
            Ok(OriginFile::Image { image, .. }) => self.shelf("image", &image, "json").is_file(),
            Err(_) => false,
        }
    }

    /// A digest's algorithm and encoded halves as a directory and a file name, which keeps
    /// the colon out of the path -- NTFS refuses one.
    fn shelf(&self, name: &str, digest: &Digest, extension: &str) -> PathBuf {
        self.root
            .join(name)
            .join(digest.algorithm().as_ref())
            .join(format!("{}.{extension}", digest.digest()))
    }

    /// Named by a hash of the canonical spelling: `alpine:3.21` and
    /// `docker.io/library/alpine:3.21` are one file, and tags differing only in case are two
    /// even on a filesystem that does not tell them apart.
    fn tag_path(&self, reference: &Reference) -> PathBuf {
        let hash = Sha256::digest(reference.to_string());
        self.root.join("tag").join(format!("{hash:x}.json"))
    }

    /// The merged metadata image and the disk of `layers`, named by a hash of the list: the
    /// same layers in the same order are one root filesystem, whichever images stack them.
    fn rootfs_paths(&self, layers: &[Digest]) -> (PathBuf, PathBuf) {
        let list = serde_json::to_vec(layers).expect("digests serialize");
        let base = self
            .root
            .join("rootfs")
            .join("sha256")
            .join(format!("{:x}", Sha256::digest(list)));
        (base.with_extension("erofs"), base.with_extension("vmdk"))
    }
}

/// Write `path` with `write`, and only then give it that name: a write that stops part-way
/// leaves nothing under it, and of two writers of one name the last rename wins.
fn write_atomically(path: &Path, write: impl FnOnce(&mut File) -> Result<()>) -> Result<()> {
    let part = write_part(path, write)?;
    fs::rename(&part, path).map_err(|e| {
        let _ = fs::remove_file(&part);
        Error::io(format!("moving {} into place", path.display()), e)
    })
}

/// Write a file beside `path` with `write`, and answer with it, for the caller to name. It is
/// this process's and this call's, so two writers -- threads of a server included -- each
/// write their own. One whose write fails is not left.
fn write_part(path: &Path, write: impl FnOnce(&mut File) -> Result<()>) -> Result<PathBuf> {
    static WRITES: AtomicU64 = AtomicU64::new(0);
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let part = path.with_file_name(format!(
        "{name}.{}.{}.part",
        std::process::id(),
        WRITES.fetch_add(1, Ordering::Relaxed)
    ));

    let mut file =
        File::create(&part).map_err(|e| Error::io(format!("creating {}", part.display()), e))?;
    let written = write(&mut file);
    // Closed before it is named: Windows renames and removes nothing that is still open.
    drop(file);
    if let Err(e) = written {
        let _ = fs::remove_file(&part);
        return Err(e);
    }
    Ok(part)
}

/// Make what is filed at `path` young again, or answer `false` when nothing is.
fn renew(path: &Path) -> io::Result<bool> {
    match File::options().write(true).open(path) {
        Ok(file) => file.set_modified(SystemTime::now()).map(|()| true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// What is filed at `path`, or `None` when nothing is: a removal is its file removed, and
/// a listing may name a file a removal takes before it is read.
fn read_filed<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Error::io(format!("reading {}", path.display()), e)),
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|e| Error::io(format!("reading {}", path.display()), e.into()))
}

#[cfg(test)]
mod tests {
    use oci_spec::image::{ImageConfigurationBuilder, MediaType};

    use super::*;
    use crate::fstree::{Entry, Meta};

    struct Scratch(Store);

    impl Scratch {
        fn new(name: &str) -> Scratch {
            let path =
                std::env::temp_dir().join(format!("virtx-uvm-store-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            Scratch(Store::new(path).unwrap())
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(self.0.root());
        }
    }

    #[test]
    fn a_layer_linked_from_another_store_is_one_file_and_outlives_its_removal_there() {
        let (one, two) = (Scratch::new("link-one"), Scratch::new("link-two"));
        let layer = one.0.insert_layer(&tree(b"shared")).unwrap();
        two.0.link_layer(&one.0.layer_path(&layer), &layer).unwrap();
        assert_eq!(
            two.0.get_layer(&layer).unwrap().unwrap().digest(),
            tree(b"shared").digest()
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            let inode = |store: &Store| fs::metadata(store.layer_path(&layer)).unwrap().ino();
            assert_eq!(inode(&one.0), inode(&two.0));
        }
        // Linked again: renewed, and still one file.
        two.0.link_layer(&one.0.layer_path(&layer), &layer).unwrap();
        assert!(one.0.remove_layer(&layer).unwrap());
        assert!(two.0.contains_layer(&layer));
        // From nowhere: refused, and nothing left half done.
        let other: Digest = format!("sha256:{}", "b".repeat(64)).parse().unwrap();
        assert!(two.0.link_layer(&one.0.layer_path(&other), &other).is_err());
        assert!(!two.0.contains_layer(&other));
        let dir = two.0.root().join("layer").join("sha256");
        assert!(
            fs::read_dir(dir).unwrap().all(|e| !e
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".link")),
        );
    }

    fn tree(content: &[u8]) -> FsTree {
        let mut tree = FsTree::new();
        tree.insert(
            b"etc/hostname",
            Entry::File {
                meta: Meta::default(),
                data: content.to_vec(),
            },
        )
        .unwrap();
        tree
    }

    fn config(author: &str) -> ImageConfiguration {
        ImageConfigurationBuilder::default()
            .author(author)
            .build()
            .unwrap()
    }

    fn reference(text: &str) -> Reference {
        text.parse().unwrap()
    }

    #[test]
    fn a_layer_comes_back_as_it_went_in() {
        let scratch = Scratch::new("layer");
        let store = &scratch.0;
        let tree = tree(b"here");

        let digest = store.insert_layer(&tree).unwrap();
        let image = fs::read(store.layer_path(&digest)).unwrap();
        assert_eq!(digest.digest(), &format!("{:x}", Sha256::digest(&image)));
        assert!(store.contains_layer(&digest));
        assert_eq!(
            store.get_layer(&digest).unwrap().unwrap().digest(),
            tree.digest()
        );
        assert_eq!(store.insert_layer(&tree).unwrap(), digest);

        assert!(store.remove_layer(&digest).unwrap());
        assert!(!store.remove_layer(&digest).unwrap());
        assert!(store.get_layer(&digest).unwrap().is_none());
    }

    #[test]
    fn an_image_needs_its_layers_and_holds_them() {
        let scratch = Scratch::new("image");
        let store = &scratch.0;
        let absent = tree(b"absent").digest();
        assert!(matches!(
            store.insert_image(&config("a"), std::slice::from_ref(&absent)),
            Err(Error::Missing(missing)) if missing == absent
        ));

        let layers = vec![
            store.insert_layer(&tree(b"one")).unwrap(),
            store.insert_layer(&tree(b"two")).unwrap(),
        ];
        let image = store.insert_image(&config("a"), &layers).unwrap();
        assert_eq!(store.insert_image(&config("a"), &layers).unwrap(), image);
        assert_ne!(store.insert_image(&config("b"), &layers).unwrap(), image);
        assert_eq!(store.images().unwrap().len(), 2);

        let mut filed = config("a");
        filed
            .rootfs_mut()
            .set_diff_ids(layers.iter().map(Digest::to_string).collect());
        assert_eq!(
            store.get_image(&image).unwrap(),
            Some((filed, layers.clone()))
        );
        let manifest: ImageManifest =
            serde_json::from_slice(&store.get_image_manifest(&image).unwrap().unwrap().0).unwrap();
        assert_eq!(manifest.layers()[0].media_type().to_string(), EROFS_LAYER);

        assert!(matches!(
            store.remove_layer(&layers[0]),
            Err(Error::InUse(..))
        ));
        assert!(store.remove_image(&image).unwrap());
        assert!(store.get_image(&image).unwrap().is_none());
    }

    #[test]
    fn a_tag_needs_its_image_and_holds_it() {
        let scratch = Scratch::new("tag");
        let store = &scratch.0;
        let layer = store.insert_layer(&tree(b"one")).unwrap();
        let image = store.insert_image(&config("a"), &[layer]).unwrap();
        let other = store.insert_image(&config("b"), &[]).unwrap();
        store.remove_image(&other).unwrap();

        assert!(matches!(
            store.insert_tag(&reference("alpine:3.21"), &other),
            Err(Error::Missing(_))
        ));
        store.insert_tag(&reference("alpine:3.21"), &image).unwrap();
        assert!(matches!(store.remove_image(&image), Err(Error::InUse(..))));

        assert_eq!(
            store.remove_tag(&reference("alpine:3.21")).unwrap(),
            Some(image.clone())
        );
        assert_eq!(store.remove_tag(&reference("alpine:3.21")).unwrap(), None);
        assert!(store.remove_image(&image).unwrap());
    }

    #[test]
    fn every_spelling_of_a_reference_is_one_tag() {
        let scratch = Scratch::new("spelling");
        let store = &scratch.0;
        let a = store.insert_image(&config("a"), &[]).unwrap();
        let b = store.insert_image(&config("b"), &[]).unwrap();

        store.insert_tag(&reference("alpine:3.21"), &a).unwrap();
        store
            .insert_tag(&reference("docker.io/library/alpine:3.21"), &b)
            .unwrap();
        store.insert_tag(&reference("alpine:Latest"), &a).unwrap();
        store.insert_tag(&reference("alpine:latest"), &b).unwrap();

        let tags = store.tags().unwrap();
        assert_eq!(tags.len(), 3, "{tags:?}");
        assert_eq!(store.get_tag(&reference("alpine:3.21")).unwrap(), Some(b));
        assert_eq!(store.get_tag(&reference("alpine:Latest")).unwrap(), Some(a));
    }

    /// A file a directory's listing names but a removal took before it is read: a dangling
    /// link reads the same way.
    #[cfg(unix)]
    fn gone(dir: &Path) {
        let gone = dir.join("gone.json");
        std::os::unix::fs::symlink(gone.with_extension("removed"), &gone).unwrap();
    }

    #[test]
    fn a_change_waits_for_the_one_under_way() {
        let scratch = Scratch::new("change");
        let store = &scratch.0;
        let image = store.insert_image(&config("a"), &[]).unwrap();
        let change = store.change().unwrap();
        let (started, start) = std::sync::mpsc::channel();
        let tagging = std::thread::spawn({
            let (store, image) = (store.clone(), image.clone());
            move || {
                started.send(()).unwrap();
                store.insert_tag(&reference("waited:latest"), &image)
            }
        });
        start.recv().unwrap();
        std::thread::sleep(Duration::from_millis(200));
        assert!(
            !tagging.is_finished(),
            "a tag was named while a change was under way"
        );
        drop(change);
        tagging.join().unwrap().unwrap();
        assert_eq!(
            store.get_tag(&reference("waited:latest")).unwrap(),
            Some(image.clone())
        );

        let change = store.change().unwrap();
        let (started, start) = std::sync::mpsc::channel();
        let removing = std::thread::spawn({
            let store = store.clone();
            move || {
                started.send(()).unwrap();
                store.remove_tag(&reference("waited:latest"))
            }
        });
        start.recv().unwrap();
        std::thread::sleep(Duration::from_millis(200));
        assert!(
            !removing.is_finished(),
            "a tag was taken away while a change was under way"
        );
        drop(change);
        assert_eq!(removing.join().unwrap().unwrap(), Some(image));
        assert_eq!(store.get_tag(&reference("waited:latest")).unwrap(), None);
    }

    fn age(path: &Path, by: Duration) {
        File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(SystemTime::now() - by)
            .unwrap();
    }

    #[test]
    fn a_layer_filed_again_or_renewed_is_young_again() {
        let scratch = Scratch::new("young-layer");
        let store = &scratch.0;
        let grace = Duration::from_secs(3600);
        let layer = store.insert_layer(&tree(b"young")).unwrap();

        age(&store.layer_path(&layer), 2 * grace);
        store.insert_layer(&tree(b"young")).unwrap();
        store.prune(grace).unwrap();
        assert!(
            store.contains_layer(&layer),
            "a layer filed again was pruned"
        );

        age(&store.layer_path(&layer), 2 * grace);
        assert!(store.renew_layer(&layer).unwrap());
        store.prune(grace).unwrap();
        assert!(store.contains_layer(&layer), "a layer renewed was pruned");

        age(&store.layer_path(&layer), 2 * grace);
        store.prune(grace).unwrap();
        assert!(!store.contains_layer(&layer));
        assert!(!store.renew_layer(&layer).unwrap());
    }

    #[test]
    fn an_image_filed_again_or_renewed_is_young_again() {
        let scratch = Scratch::new("young-image");
        let store = &scratch.0;
        let grace = Duration::from_secs(3600);
        let image = store.insert_image(&config("young"), &[]).unwrap();
        let path = store.shelf("image", &image, "json");

        age(&path, 2 * grace);
        store.insert_image(&config("young"), &[]).unwrap();
        store.prune(grace).unwrap();
        assert!(
            store.get_image(&image).unwrap().is_some(),
            "an image filed again was pruned"
        );

        age(&path, 2 * grace);
        assert!(store.renew_image(&image).unwrap());
        store.prune(grace).unwrap();
        assert!(
            store.get_image(&image).unwrap().is_some(),
            "an image renewed was pruned"
        );

        age(&path, 2 * grace);
        store.prune(grace).unwrap();
        assert!(store.get_image(&image).unwrap().is_none());
        assert!(!store.renew_image(&image).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn a_tag_removed_while_listing_is_left_out() {
        let scratch = Scratch::new("listing");
        let store = &scratch.0;
        let image = store.insert_image(&config("a"), &[]).unwrap();
        store.insert_tag(&reference("kept:latest"), &image).unwrap();
        gone(&store.root().join("tag"));

        let tags = store.tags().unwrap();
        assert_eq!(tags.len(), 1, "{tags:?}");
        assert_eq!(tags[0].0.to_string(), "docker.io/library/kept:latest");
    }

    #[cfg(unix)]
    #[test]
    fn an_origin_removed_while_listing_is_left_out() {
        let scratch = Scratch::new("origin-listing");
        let store = &scratch.0;
        let digest = |hex: char| -> Digest {
            format!("sha256:{}", hex.to_string().repeat(64))
                .parse()
                .unwrap()
        };
        let blob = Descriptor::new(MediaType::ImageLayerGzip, 1, digest('a'));
        let diff_id = digest('1');
        let layer = store.insert_layer(&tree(b"one")).unwrap();
        store.insert_layer_origin(&blob, &diff_id, &layer).unwrap();
        gone(&store.root().join("origin").join("sha256"));

        assert_eq!(
            store.layer_origins_of(&layer).unwrap(),
            vec![(blob, diff_id)]
        );
        assert_eq!(store.image_origins_of(&layer).unwrap(), vec![]);
    }

    #[test]
    fn a_prune_takes_what_nothing_needs() {
        let scratch = Scratch::new("prune");
        let store = &scratch.0;
        let digest = |hex: char| -> Digest {
            format!("sha256:{}", hex.to_string().repeat(64))
                .parse()
                .unwrap()
        };
        let image_of = |content: &[u8], author: &str| {
            let layer = store.insert_layer(&tree(content)).unwrap();
            let image = store
                .insert_image(&config(author), std::slice::from_ref(&layer))
                .unwrap();
            store.insert_rootfs(std::slice::from_ref(&layer)).unwrap();
            store
                .insert_layer_origin(
                    &Descriptor::new(MediaType::ImageLayerGzip, 1, layer.clone()),
                    &digest('1'),
                    &layer,
                )
                .unwrap();
            (layer, image)
        };
        let (tagged_layer, tagged) = image_of(b"tagged", "a");
        store.insert_tag(&reference("app:v1"), &tagged).unwrap();
        let (held_layer, held) = image_of(b"held", "b");
        let hold = store.hold_image(&held).unwrap();
        let (loose_layer, loose) = image_of(b"loose", "c");

        let young = store.prune(Duration::from_secs(3600)).unwrap();
        assert!(young.images.is_empty(), "nothing is old yet");

        let pruned = store.prune(Duration::ZERO).unwrap();
        assert_eq!(pruned.images, vec![loose.clone()]);
        assert!(pruned.reclaimed > 0);
        assert!(store.get_image(&loose).unwrap().is_none());
        assert!(!store.contains_layer(&loose_layer));
        assert!(
            store
                .get_rootfs(std::slice::from_ref(&loose_layer))
                .is_none()
        );
        assert_eq!(store.get_layer_origin(&loose_layer).unwrap(), None);
        for (layer, image) in [(&tagged_layer, &tagged), (&held_layer, &held)] {
            assert!(store.get_image(image).unwrap().is_some());
            assert!(store.contains_layer(layer));
            assert!(store.get_rootfs(std::slice::from_ref(layer)).is_some());
        }
        assert!(matches!(store.remove_image(&held), Err(Error::InUse(..))));

        drop(hold);
        assert_eq!(store.prune(Duration::ZERO).unwrap().images, vec![held]);
    }

    #[test]
    fn a_layer_origin_finds_its_layer_while_it_is_here() {
        let scratch = Scratch::new("origin");
        let store = &scratch.0;
        let digest = |hex: char| -> Digest {
            format!("sha256:{}", hex.to_string().repeat(64))
                .parse()
                .unwrap()
        };
        let gzip = Descriptor::new(MediaType::ImageLayerGzip, 1, digest('a'));
        let zstd = Descriptor::new(MediaType::ImageLayerZstd, 1, digest('b'));
        let layer = store.insert_layer(&tree(b"one")).unwrap();
        store.remove_layer(&layer).unwrap();

        assert!(matches!(
            store.insert_layer_origin(&gzip, &digest('1'), &layer),
            Err(Error::Missing(_))
        ));
        assert_eq!(store.insert_layer(&tree(b"one")).unwrap(), layer);
        store
            .insert_layer_origin(&gzip, &digest('1'), &layer)
            .unwrap();
        store
            .insert_layer_origin(&zstd, &digest('1'), &layer)
            .unwrap();

        assert_eq!(
            store.get_layer_origin(&digest('a')).unwrap(),
            Some(layer.clone())
        );
        assert_eq!(
            store.get_layer_origin(&digest('1')).unwrap(),
            None,
            "found by a diff_id"
        );
        let mut origins = store.layer_origins_of(&layer).unwrap();
        origins.sort_by_key(|(descriptor, _)| descriptor.digest().to_string());
        assert_eq!(origins, vec![(gzip, digest('1')), (zstd, digest('1'))]);

        store.remove_layer(&layer).unwrap();
        assert_eq!(store.get_layer_origin(&digest('a')).unwrap(), None);
    }

    #[test]
    fn an_image_origin_finds_its_image_while_it_is_here() {
        let scratch = Scratch::new("image-origin");
        let store = &scratch.0;
        // Spaced as no serializer would, so that only the bytes as given come back.
        let manifest = br#"{ "schemaVersion" : 2 }"#;
        let served = br#"{ "os" : "linux" }"#;
        let named: Digest = format!("sha256:{}", "a".repeat(64)).parse().unwrap();
        let image = store.insert_image(&config("a"), &[]).unwrap();
        store.remove_image(&image).unwrap();

        assert!(matches!(
            store.insert_image_origin(&named, manifest, served, &image),
            Err(Error::Missing(_))
        ));
        store.insert_image(&config("a"), &[]).unwrap();
        store
            .insert_image_origin(&named, manifest, served, &image)
            .unwrap();

        assert_eq!(store.get_image_origin(&named).unwrap(), Some(image.clone()));
        assert_eq!(
            store.get_layer_origin(&named).unwrap(),
            None,
            "found as a blob"
        );
        assert_eq!(
            store.image_origins_of(&image).unwrap(),
            vec![(manifest.to_vec(), served.to_vec())]
        );

        store.remove_image(&image).unwrap();
        assert_eq!(store.get_image_origin(&named).unwrap(), None);
    }

    /// The disk a descriptor describes, read the way a VM would: every extent it names, in
    /// order, end to end.
    fn read_disk(descriptor: &Path) -> Vec<u8> {
        let text = fs::read_to_string(descriptor).unwrap();
        let mut disk = Vec::new();
        for line in text.lines().filter(|line| line.starts_with("RW ")) {
            let mut fields = line.split('"');
            let sectors: usize = fields
                .next()
                .unwrap()
                .split(' ')
                .nth(1)
                .unwrap()
                .parse()
                .unwrap();
            let path = fields.next().unwrap();
            let offset: usize = fields.next().unwrap().trim().parse().unwrap();
            disk.extend_from_slice(&fs::read(path).unwrap()[offset * 512..][..sectors * 512]);
        }
        disk
    }

    #[test]
    fn a_rootfs_reads_as_its_layers_stacked() {
        let scratch = Scratch::new("rootfs");
        let store = &scratch.0;
        let mut lower = tree(b"lower");
        lower
            .insert(
                b"bin/tool",
                Entry::File {
                    meta: Meta::default(),
                    data: vec![7; 5000],
                },
            )
            .unwrap();
        lower.link(b"bin/alias", b"bin/tool").unwrap();
        let mut upper = tree(b"upper");
        upper.insert(b"bin/tool", Entry::Whiteout).unwrap();
        let layers = [
            store.insert_layer(&lower).unwrap(),
            store.insert_layer(&upper).unwrap(),
        ];
        assert!(store.get_rootfs(&layers).is_none());

        let disk = store.insert_rootfs(&layers).unwrap();
        assert_eq!(store.get_rootfs(&layers), Some(disk.clone()));
        assert_eq!(store.insert_rootfs(&layers).unwrap(), disk);

        let read = FsTree::from_erofs(io::Cursor::new(read_disk(&disk))).unwrap();
        let mut stacked = FsTree::new();
        stacked.stack(lower);
        stacked.stack(upper);
        assert_eq!(read.digest(), stacked.digest());
        assert_ne!(
            store.rootfs_paths(&layers),
            store.rootfs_paths(&[layers[1].clone(), layers[0].clone()]),
            "two orders of the same layers are two filesystems"
        );

        assert!(store.remove_rootfs(&layers).unwrap());
        assert!(!store.remove_rootfs(&layers).unwrap());
        assert!(store.get_rootfs(&layers).is_none());
    }

    /// A root filesystem points into its layers' files, so one with a layer gone is not
    /// there -- and is again once the layer is put back, being the same file.
    #[test]
    fn a_rootfs_is_there_while_its_layers_are() {
        let scratch = Scratch::new("rootfs-layers");
        let store = &scratch.0;
        let layer = store.insert_layer(&tree(b"one")).unwrap();
        let disk = store.insert_rootfs(std::slice::from_ref(&layer)).unwrap();

        store.remove_layer(&layer).unwrap();
        assert!(store.get_rootfs(std::slice::from_ref(&layer)).is_none());
        assert!(matches!(
            store.insert_rootfs(std::slice::from_ref(&layer)),
            Err(Error::Missing(_))
        ));

        store.insert_layer(&tree(b"one")).unwrap();
        assert_eq!(store.get_rootfs(std::slice::from_ref(&layer)), Some(disk));
    }

    #[test]
    fn a_part_written_file_is_not_listed() {
        let scratch = Scratch::new("part");
        let store = &scratch.0;
        let image = store.insert_image(&config("a"), &[]).unwrap();
        let path = store.shelf("image", &image, "json");
        fs::write(path.with_extension("json.1.0.part"), b"half").unwrap();

        assert_eq!(store.images().unwrap(), vec![image]);
    }
}
