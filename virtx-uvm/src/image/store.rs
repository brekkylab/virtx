//! What this host keeps of images: layers, the images stacked from them, and the tags that
//! name images.
//!
//! # Layout
//!
//! ```text
//! <root>/
//!   layer/sha256/<hex>.erofs   one per layer, named by its tree's digest
//!   image/sha256/<hex>.json    one per image: its config and its layers, named by this file's digest
//!   rootfs/sha256/<hex>.erofs  one per list of layers, named by the list's digest: them merged
//!                 <hex>.vmdk   ... and the disk of that image and the layers it points into
//!   tag/<hex>.json             one per reference: which image it names
//!   origin/sha256/<hex>.json   one per registry blob, named by its digest: which layer it became
//! ```
//!
//! Each shelf names only what is on the one below it: an image is refused unless its layers
//! are here, a tag unless its image is, and nothing is removed while something still names
//! it. Nothing lands under a name except by a rename once it is whole.
//!
//! An origin and a root filesystem are the exceptions: neither holds its layers, and one
//! whose layers are not all here is read as not there. A layer put back is the same file
//! it was, so what was made of it is whole again.
//!
//! Not locked: a check and the write after it are two steps, and another thread or process
//! can act between them.

use std::fs::{self, File};
use std::io::{self, BufReader, Write};
use std::iter;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use oci_spec::distribution::Reference;
use oci_spec::image::{Descriptor, Digest, ImageConfiguration, Sha256Digest};
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

#[derive(Serialize, Deserialize)]
struct ImageFile {
    config: ImageConfiguration,
    layers: Vec<Digest>,
}

#[derive(Serialize, Deserialize)]
struct TagFile {
    reference: String,
    image: Digest,
}

#[derive(Serialize, Deserialize)]
struct OriginFile {
    layer: Digest,
    diff_id: Digest,
    descriptor: Descriptor,
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
            root.join("image").join("sha256"),
            root.join("tag"),
            root.join("origin").join("sha256"),
            root.join("rootfs").join("sha256"),
        ] {
            fs::create_dir_all(&dir)
                .map_err(|e| Error::io(format!("making {}", dir.display()), e))?;
        }
        Ok(Store { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn layer_path(&self, layer: &Digest) -> PathBuf {
        self.shelf("layer", layer, "erofs")
    }

    pub fn contains_layer(&self, layer: &Digest) -> bool {
        self.layer_path(layer).is_file()
    }

    /// File `tree` as a layer under its digest, which is returned. A layer already here is
    /// left as it is.
    pub fn insert_layer(&self, tree: &FsTree) -> Result<Digest> {
        let digest = tree.digest();
        let path = self.layer_path(&digest);
        if !path.is_file() {
            write_atomically(&path, |file| Ok(tree.to_erofs(file)?))?;
        }
        Ok(digest)
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

    /// File an image of `layers`, bottom first, all of which must be here. Its digest is
    /// that of the file it is kept in, which is returned.
    pub fn insert_image(&self, config: &ImageConfiguration, layers: &[Digest]) -> Result<Digest> {
        if let Some(missing) = layers.iter().find(|layer| !self.contains_layer(layer)) {
            return Err(Error::Missing(missing.clone()));
        }
        let bytes = serde_json::to_vec(&ImageFile {
            config: config.clone(),
            layers: layers.to_vec(),
        })
        .map_err(|e| Error::io("serializing an image", e.into()))?;
        let digest: Digest = format!("{:x}", Sha256::digest(&bytes))
            .parse::<Sha256Digest>()
            .expect("sha2 writes 64 lowercase hex characters")
            .into();

        let path = self.shelf("image", &digest, "json");
        if !path.is_file() {
            write_atomically(&path, |file| {
                file.write_all(&bytes)
                    .map_err(|e| Error::io(format!("writing {}", path.display()), e))
            })?;
        }
        Ok(digest)
    }

    /// An image's config and its layers, bottom first.
    pub fn get_image(&self, image: &Digest) -> Result<Option<(ImageConfiguration, Vec<Digest>)>> {
        let path = self.shelf("image", image, "json");
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(Error::io(format!("reading {}", path.display()), e)),
        };
        let read: ImageFile = serde_json::from_slice(&bytes)
            .map_err(|e| Error::io(format!("reading {}", path.display()), e.into()))?;
        Ok(Some((read.config, read.layers)))
    }

    /// Delete an image no tag names. Its layers stay. `false` if it was not here.
    pub fn remove_image(&self, image: &Digest) -> Result<bool> {
        if let Some((reference, _)) = self.tags()?.into_iter().find(|(_, named)| named == image) {
            return Err(Error::InUse(image.clone(), reference.to_string()));
        }
        let path = self.shelf("image", image, "json");
        match fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(Error::io(format!("removing {}", path.display()), e)),
        }
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
        let path = self.tag_path(reference);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(Error::io(format!("reading {}", path.display()), e)),
        };
        let read: TagFile = serde_json::from_slice(&bytes)
            .map_err(|e| Error::io(format!("reading {}", path.display()), e.into()))?;
        Ok(Some(read.image))
    }

    /// Take `reference` away, and say what it named.
    pub fn remove_tag(&self, reference: &Reference) -> Result<Option<Digest>> {
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
            let bytes =
                fs::read(&path).map_err(|e| Error::io(format!("reading {}", path.display()), e))?;
            let read: TagFile = serde_json::from_slice(&bytes)
                .map_err(|e| Error::io(format!("reading {}", path.display()), e.into()))?;
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
    pub fn insert_origin(
        &self,
        descriptor: &Descriptor,
        diff_id: &Digest,
        layer: &Digest,
    ) -> Result<()> {
        if !self.contains_layer(layer) {
            return Err(Error::Missing(layer.clone()));
        }
        let path = self.shelf("origin", descriptor.digest(), "json");
        let bytes = serde_json::to_vec(&OriginFile {
            layer: layer.clone(),
            diff_id: diff_id.clone(),
            descriptor: descriptor.clone(),
        })
        .map_err(|e| Error::io("serializing an origin", e.into()))?;
        write_atomically(&path, |file| {
            file.write_all(&bytes)
                .map_err(|e| Error::io(format!("writing {}", path.display()), e))
        })
    }

    /// The layer the registry blob `blob` became, if it is still here.
    pub fn get_origin(&self, blob: &Digest) -> Result<Option<Digest>> {
        let path = self.shelf("origin", blob, "json");
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(Error::io(format!("reading {}", path.display()), e)),
        };
        let read: OriginFile = serde_json::from_slice(&bytes)
            .map_err(|e| Error::io(format!("reading {}", path.display()), e.into()))?;
        Ok(self.contains_layer(&read.layer).then_some(read.layer))
    }

    /// Every registry blob that became `layer`, with the diff_id its config named it by:
    /// what a push can name in place of uploading `layer` again.
    pub fn origins_of(&self, layer: &Digest) -> Result<Vec<(Descriptor, Digest)>> {
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
            let bytes =
                fs::read(&path).map_err(|e| Error::io(format!("reading {}", path.display()), e))?;
            let read: OriginFile = serde_json::from_slice(&bytes)
                .map_err(|e| Error::io(format!("reading {}", path.display()), e.into()))?;
            if read.layer == *layer {
                origins.push((read.descriptor, read.diff_id));
            }
        }
        Ok(origins)
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
/// leaves nothing under it. The file beside it is this process's and this call's, so two
/// writers of one name -- threads of a server included -- each write their own and the last
/// rename wins.
fn write_atomically(path: &Path, write: impl FnOnce(&mut File) -> Result<()>) -> Result<()> {
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
    // Closed before the rename: Windows renames and removes nothing that is still open.
    drop(file);

    let renamed = written.and_then(|()| {
        fs::rename(&part, path)
            .map_err(|e| Error::io(format!("moving {} into place", path.display()), e))
    });
    if renamed.is_err() {
        let _ = fs::remove_file(&part);
    }
    renamed
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
        assert_eq!(digest, tree.digest());
        assert!(store.contains_layer(&digest));
        assert_eq!(store.get_layer(&digest).unwrap().unwrap().digest(), digest);
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

        assert_eq!(
            store.get_image(&image).unwrap(),
            Some((config("a"), layers.clone()))
        );

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

    #[test]
    fn an_origin_finds_its_layer_while_it_is_here() {
        let scratch = Scratch::new("origin");
        let store = &scratch.0;
        let digest = |hex: char| -> Digest {
            format!("sha256:{}", hex.to_string().repeat(64))
                .parse()
                .unwrap()
        };
        let gzip = Descriptor::new(MediaType::ImageLayerGzip, 1, digest('a'));
        let zstd = Descriptor::new(MediaType::ImageLayerZstd, 1, digest('b'));
        let layer = tree(b"one").digest();

        assert!(matches!(
            store.insert_origin(&gzip, &digest('1'), &layer),
            Err(Error::Missing(_))
        ));
        store.insert_layer(&tree(b"one")).unwrap();
        store.insert_origin(&gzip, &digest('1'), &layer).unwrap();
        store.insert_origin(&zstd, &digest('1'), &layer).unwrap();

        assert_eq!(store.get_origin(&digest('a')).unwrap(), Some(layer.clone()));
        assert_eq!(
            store.get_origin(&digest('1')).unwrap(),
            None,
            "found by a diff_id"
        );
        let mut origins = store.origins_of(&layer).unwrap();
        origins.sort_by_key(|(descriptor, _)| descriptor.digest().to_string());
        assert_eq!(origins, vec![(gzip, digest('1')), (zstd, digest('1'))]);

        store.remove_layer(&layer).unwrap();
        assert_eq!(store.get_origin(&digest('a')).unwrap(), None);
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
