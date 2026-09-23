//! Making a cached image into a disk a guest can boot.
//!
//! # What comes out
//!
//! One block device, read as one EROFS filesystem, made of files that are already
//! here:
//!
//! ```text
//! <stack>.vmdk  =  [ <stack>.fsmeta.erofs │ layer 0 │ layer 1 │ … ]
//!                     │
//!                     └── every inode of the merged image, each file's data address
//!                         pointing into the layer its bytes are still sitting in
//! ```
//!
//! The guest mounts `/dev/vda` and finds a filesystem. It is not an overlay and there
//! is no stack left in it: the layers were merged here -- later ones winning,
//! whiteouts deleting, opaque directories emptying -- and what the metadata image
//! describes is the image as a single tree.
//!
//! # What is not copied
//!
//! Any layer's contents. A stitched disk costs one metadata image and a few hundred
//! bytes of descriptor, whatever the layers weigh, and two images over one base share
//! that base rather than each holding a copy. That is the whole reason the layer
//! images are written the way they are: a file keeps every byte of itself in its
//! layer's data area, in whole blocks, so a merged image can point at it.
//!
//! # What has to agree
//!
//! Two accounts of where a layer's bytes are. The addresses written into the metadata
//! put layer *n* after the metadata image and after every layer before it, each one as
//! long as its own superblock says; the descriptor puts it after however many bytes
//! those files actually hold. If the two ever came apart nothing downstream would say
//! so -- the guest would mount, read at the wrong offset and find another layer's
//! bytes -- so every layer is checked against its own image as it is read, and the
//! metadata image is checked against the length its addresses were computed from.
//!
//! The descriptor names the files absolutely, because a VM reads it from wherever it
//! happens to stand. A cache that is moved is one whose disks have to be made again.

use std::fmt::Display;
use std::fs::File;
use std::path::{Path, PathBuf};

use crate::image::disk::write_descriptor;
use crate::image::erofs::{self, BLOCK_SIZE};
use crate::image::{Cache, Digest, Disk, Error, Image, Reference, Result, stack};

/// Put the layers of `reference` together into a disk, and say where it is.
///
/// `reference` is anything a [`Reference`] can be read out of -- `"alpine:3.21"`, a
/// `String`, or one already parsed -- and it is read the way a registry reads it, so
/// every spelling of one image is that image: `alpine:3.21`,
/// `docker.io/library/alpine:3.21` and `alpine@sha256:0a7e...` all name the same thing
/// here.
///
/// The image has to be in the cache already -- this asks no registry anything, and an
/// image that was never pulled is an error rather than a download. `None` for the
/// cache takes the same default [`pull`](super::pull) writes to.
///
/// A disk already stitched is not made again: it is named by the layers it was made
/// of, so a name that exists is the same disk. What is re-read each time is only
/// whether the two files are there.
pub fn provision<E: Display>(
    reference: impl TryInto<Reference, Error = E>,
    cache: Option<Cache>,
) -> Result<Disk> {
    let cache = cache.unwrap_or_default();

    // Parsed and then printed again rather than looked up as it was written: the index is
    // keyed by the canonical spelling, so `alpine:3.21` finds what
    // `docker.io/library/alpine:3.21` pulled.
    let reference = reference
        .try_into()
        .map_err(|e| Error::Reference(e.to_string()))?;
    let name = reference.to_string();

    let index = cache.image_index()?;

    // The name it was pulled under, and failing that the digest it was pulled as. A
    // manifest digest names the image rather than the way somebody reached it, so
    // `repo@sha256:...` finds what arrived under a tag -- and finds it whatever
    // repository it is spelled with, the digest being the stronger of the two names.
    let image = index
        .get(&name)
        .or_else(|| {
            let digest = Digest::parse(reference.digest()?).ok()?;
            index.values().find(|image| image.manifest == digest)
        })
        .ok_or_else(|| Error::NotPulled(name.clone()))?;

    stitch(image, &cache)
}

/// The same for an image in hand, which is what a caller that just pulled one has.
fn stitch(image: &Image, cache: &Cache) -> Result<Disk> {
    if image.layers.is_empty() {
        return Err(Error::Disk(
            "an image with no layers is not a filesystem to boot".into(),
        ));
    }

    // What the disk is called -- see [`stack::digest`]. Not kept on the `Disk`: a boot has
    // no use for it, and it is in the cache for anything that does.
    let stack = stack::digest(&image.layers);
    let disk = Disk {
        path: cache.disk_path(&stack),
        config: image.config.clone(),
    };
    if cache.has_disk(&stack) {
        return Ok(disk);
    }

    // Every layer's tree, each moved onto the disk as it is read: its bytes past the
    // layers already placed, its inode ids past the ids they have spoken for. Both are
    // needed before the trees can be merged -- two layers addressing from zero would
    // overlap, and two layers numbering from zero would have the merged image hand one
    // file's bytes to another file's name.
    //
    // Only the front of the disk is still unknown here, because how far the metadata
    // image pushes the layers along depends on the merged tree, which does not exist
    // yet. That much is added to every file at the end.
    let mut trees = Vec::with_capacity(image.layers.len());
    let mut layers = Vec::with_capacity(image.layers.len());
    let (mut at, mut ids) = (0u64, 0u32);

    for layer in &image.layers {
        let path = cache.layer_path(&layer.digest);
        let mut file =
            File::open(&path).map_err(|e| Error::io(format!("opening {}", path.display()), e))?;

        let (mut tree, layout) =
            erofs::read(&mut file).map_err(|e| Error::Layer(path.display().to_string(), e))?;

        // The one thing nothing further along could catch. Everything stacked behind
        // this layer is addressed as if the layer were the length its superblock says,
        // so a file that is some other length puts every layer after it somewhere the
        // metadata does not point -- silently, as another layer's bytes.
        let size = file
            .metadata()
            .map_err(|e| Error::io(format!("reading {}", path.display()), e))?
            .len();
        if size != layout.bytes {
            return Err(Error::Disk(format!(
                "{} is {size} bytes and its superblock accounts for {} -- \
                 the two no longer describe the same layer",
                path.display(),
                layout.bytes
            )));
        }

        ids = stack::place(&mut tree, at, ids);
        at += layout.bytes;
        trees.push(tree);
        layers.push(path);
    }

    let mut merged = stack::merge(trees);

    // The length of the metadata image, which is what the layers behind it start
    // after. Settled before anything is written, because it depends on how many inodes
    // and directory blocks the merged tree has and not on what any of them point at --
    // so moving the tree past it cannot change it.
    let front = erofs::meta_blocks(&merged).map_err(Error::Erofs)?;
    stack::place(&mut merged, u64::from(front) * BLOCK_SIZE, 0);

    let blocks = u32::try_from(u64::from(front) + at / BLOCK_SIZE)
        .map_err(|_| Error::Disk("the stack is longer than a disk can be".into()))?;

    let fsmeta = cache.fsmeta_path(&stack);
    let mut staged = cache.staged_writer(&fsmeta)?;
    let written = erofs::write_meta(&merged, blocks, &mut staged).map_err(Error::Erofs)?;
    // The other half of the check above: the addresses were computed from `front`, and
    // an image that came out some other length has moved every layer under them.
    if written.blocks != front {
        return Err(Error::Disk(format!(
            "the merged metadata was planned as {front} blocks and written as {} -- \
             every layer behind it is addressed one block out",
            written.blocks
        )));
    }
    staged.commit()?;

    // The descriptor last, so that a name under `disk/` is never a disk whose metadata
    // is not there yet. Its extent order *is* the order the addresses above assume.
    let extents: Vec<&Path> = std::iter::once(fsmeta.as_path())
        .chain(layers.iter().map(PathBuf::as_path))
        .collect();
    let mut staged = cache.staged_writer(&disk.path)?;
    write_descriptor(&extents, &mut staged)?;
    staged.commit()?;

    Ok(disk)
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::image::Layer;
    use crate::image::tree::{Directory, Extent, Meta, Node, Tree};

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Scratch {
            let path = std::env::temp_dir().join(format!(
                "cortex-krun-provision-{name}-{}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            Scratch(path)
        }

        fn cache(&self) -> Cache {
            Cache::at(&self.0)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn meta() -> Meta {
        Meta {
            mode: 0o644,
            uid: 0,
            gid: 0,
            mtime: 0,
            xattrs: Vec::new(),
        }
    }

    fn whiteout() -> Node {
        Node::Device {
            meta: Meta { mode: 0, ..meta() },
            block: false,
            major: 0,
            minor: 0,
        }
    }

    /// Write a layer into the cache, its files holding the contents they are named
    /// with, and hand back the layer that names it.
    fn layer(
        cache: &Cache,
        name: &str,
        files: &[(&[u8], &[u8])],
        extra: &[(&[u8], Node)],
    ) -> Layer {
        let mut tree = Tree::new();
        let mut tar = Vec::new();

        for (path, contents) in files {
            let offset = tar.len() as u64;
            tar.extend_from_slice(contents);
            let id = tree.next_inode();
            tree.insert(
                path,
                Node::File {
                    meta: meta(),
                    id,
                    data: Extent {
                        offset,
                        len: contents.len() as u64,
                    },
                },
            )
            .unwrap();
        }
        for (path, node) in extra {
            tree.insert(path, node.clone()).unwrap();
        }

        let digest = Digest::of(name.as_bytes());
        let mut staged = cache.staged_writer(cache.layer_path(&digest)).unwrap();
        erofs::write(&tree, &mut Cursor::new(tar), &mut staged).unwrap();
        staged.commit().unwrap();

        Layer { digest }
    }

    /// An image over `layers`, filed in the cache under `reference`. Handed back with
    /// the image, because what the cache names a stack is not on the `Disk`.
    fn pulled(cache: &Cache, reference: &str, layers: Vec<Layer>) -> (String, Image) {
        let image = Image {
            manifest: Digest::of(reference.as_bytes()),
            layers,
            config: Default::default(),
            platform: "linux/arm64".to_string(),
        };

        let reference: Reference = reference.parse().unwrap();
        let mut index = cache.image_index_mut().unwrap();
        index.insert(reference.to_string(), image.clone());
        index.commit().unwrap();

        (reference.to_string(), image)
    }

    /// The files a descriptor names, in the order it names them.
    fn extents(descriptor: &Path) -> Vec<PathBuf> {
        std::fs::read_to_string(descriptor)
            .unwrap()
            .lines()
            .filter(|line| line.starts_with("RW "))
            .filter_map(|line| line.split('"').nth(1).map(PathBuf::from))
            .collect()
    }

    /// The disk itself: the extents laid end to end, which is what the VM reads
    /// through the descriptor and what the guest kernel mounts.
    fn disk(descriptor: &Path) -> Vec<u8> {
        let mut bytes = Vec::new();
        for extent in extents(descriptor) {
            bytes.extend_from_slice(&std::fs::read(extent).unwrap());
        }
        bytes
    }

    fn base(cache: &Cache) -> Layer {
        layer(
            cache,
            "base",
            &[
                (b"etc/keep", b"kept\n"),
                (b"etc/replaced", b"the old one\n"),
                (b"etc/deleted", b"doomed\n"),
                (b"var/cache/stale", b"stale\n"),
            ],
            &[],
        )
    }

    fn over(cache: &Cache) -> Layer {
        let mut emptied = Directory::new(meta());
        emptied.meta.set_xattr(b"trusted.overlay.opaque", b"y");

        layer(
            cache,
            "over",
            &[
                (b"etc/replaced", b"the new one, which is longer\n"),
                (b"etc/added", b"added\n"),
            ],
            &[
                (b"etc/deleted", whiteout()),
                (b"var/cache", Node::Directory(emptied)),
            ],
        )
    }

    /// The whole of it, end to end: what the guest mounts is one EROFS, it holds the
    /// layers merged, and every file in it reads back the bytes that layer put there.
    ///
    /// The disk is assembled here the way the VM assembles it -- the extents the
    /// descriptor names, in that order -- and then read as one image. Nothing about
    /// how it was written is consulted: if an address were a block out, the contents
    /// below would come back as another file's.
    #[test]
    fn a_stack_is_one_filesystem_holding_what_its_layers_put_in_it() {
        let scratch = Scratch::new("stack");
        let cache = scratch.cache();
        let (reference, _) = pulled(&cache, "test/stack:1", vec![base(&cache), over(&cache)]);

        let provisioned = provision(reference.as_str(), Some(cache.clone())).unwrap();
        assert!(provisioned.path.is_file(), "no descriptor was written");

        // The metadata image first, then the layers it points into, bottom first.
        // Compared through `canonicalize`, because a descriptor names its extents the
        // way the host resolves them and a temporary directory is a symlink here.
        let named = extents(&provisioned.path);
        let same = |named: &Path, expected: PathBuf| {
            assert_eq!(named, std::fs::canonicalize(expected).unwrap());
        };
        assert_eq!(named.len(), 3, "{named:?}");
        same(&named[1], cache.layer_path(&Digest::of(b"base")));
        same(&named[2], cache.layer_path(&Digest::of(b"over")));

        let bytes = disk(&provisioned.path);
        let (tree, layout) = erofs::read(&mut Cursor::new(bytes.clone())).unwrap();
        assert_eq!(
            layout.bytes,
            bytes.len() as u64,
            "the superblock does not describe the whole disk"
        );

        let contents = |path: &[u8]| -> Vec<u8> {
            match tree.get(path) {
                Some(Node::File { data, .. }) => {
                    bytes[data.offset as usize..(data.offset + data.len) as usize].to_vec()
                }
                other => panic!("{} is {other:?}", String::from_utf8_lossy(path)),
            }
        };

        assert_eq!(contents(b"etc/keep"), b"kept\n");
        assert_eq!(contents(b"etc/added"), b"added\n");
        assert_eq!(
            contents(b"etc/replaced"),
            b"the new one, which is longer\n",
            "the lower layer's version won"
        );
        assert!(
            tree.get(b"etc/deleted").is_none(),
            "the whiteout did not delete"
        );
        assert!(
            tree.get(b"var/cache/stale").is_none(),
            "the opaque directory did not empty"
        );
        assert!(tree.dir(b"var/cache").is_some(), "and took itself with it");
        assert!(
            tree.dir(b"var/cache")
                .unwrap()
                .meta
                .xattrs
                .iter()
                .all(|xattr| xattr.name != b"trusted.overlay.opaque"),
            "a root a guest mounts still carries an overlay marker"
        );
    }

    /// A disk is named by its layers, so the second call is the first call's answer
    /// and nothing is written again.
    #[test]
    fn a_stack_already_put_together_is_not_made_twice() {
        let scratch = Scratch::new("again");
        let cache = scratch.cache();
        let (reference, _) = pulled(&cache, "test/again:1", vec![base(&cache), over(&cache)]);

        let first = provision(reference.as_str(), Some(cache.clone())).unwrap();
        let written = std::fs::metadata(&first.path).unwrap().modified().unwrap();

        let again = provision(reference.as_str(), Some(cache.clone())).unwrap();
        assert_eq!(again.path, first.path);
        assert_eq!(
            std::fs::metadata(&again.path).unwrap().modified().unwrap(),
            written,
            "the descriptor was written again"
        );
    }

    /// One layer is a stack too, and its bytes are still read out of the layer file.
    #[test]
    fn one_layer_stitches() {
        let scratch = Scratch::new("one");
        let cache = scratch.cache();
        let (reference, _) = pulled(&cache, "test/one:1", vec![base(&cache)]);

        let provisioned = provision(reference.as_str(), Some(cache.clone())).unwrap();
        let bytes = disk(&provisioned.path);
        let (tree, _) = erofs::read(&mut Cursor::new(bytes.clone())).unwrap();

        match tree.get(b"etc/deleted") {
            Some(Node::File { data, .. }) => assert_eq!(
                &bytes[data.offset as usize..(data.offset + data.len) as usize],
                b"doomed\n"
            ),
            other => panic!("{other:?}"),
        }
    }

    /// The silent one, made loud: a layer that is no longer the length its superblock
    /// says puts every layer behind it somewhere the metadata does not point.
    #[test]
    fn a_layer_that_no_longer_matches_its_image_is_refused() {
        use std::io::Write as _;

        let scratch = Scratch::new("stale");
        let cache = scratch.cache();
        let (bottom, top) = (base(&cache), over(&cache));
        let (reference, _) = pulled(&cache, "test/stale:1", vec![bottom.clone(), top]);

        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(cache.layer_path(&bottom.digest))
            .unwrap();
        file.write_all(&[0u8; BLOCK_SIZE as usize]).unwrap();
        drop(file);

        let error = provision(reference.as_str(), Some(cache.clone())).unwrap_err();
        assert!(
            matches!(&error, Error::Disk(what) if what.contains("the same layer")),
            "{error}"
        );
        // Refused before anything was written, rather than written wrong.
        assert!(
            !cache.root().join("disk").exists(),
            "a disk was put together anyway"
        );
    }

    /// A digest names the image and not the way somebody reached it: filed under a
    /// tag, asked for as `repo@sha256:...`, and the same disk comes back.
    #[test]
    fn an_image_is_found_by_the_digest_it_was_pulled_as() {
        let scratch = Scratch::new("digest");
        let cache = scratch.cache();
        let (tagged, image) = pulled(&cache, "test/digest:1", vec![base(&cache)]);

        let by_tag = provision(tagged.as_str(), Some(cache.clone())).unwrap();
        let by_digest = provision(
            format!("test/digest@{}", image.manifest),
            Some(cache.clone()),
        )
        .unwrap();

        assert_eq!(by_digest.path, by_tag.path);
    }

    /// Every way of holding a name is a way of passing one. What the disk comes out as
    /// the tests above already say; this is here so that all three spellings compile.
    #[test]
    fn a_name_is_taken_however_the_caller_holds_it() {
        let scratch = Scratch::new("spellings");
        let cache = scratch.cache();
        let (name, _) = pulled(&cache, "test/spellings:1", vec![base(&cache)]);
        let parsed: Reference = name.parse().unwrap();

        let by_str = provision(name.as_str(), Some(cache.clone())).unwrap();
        let by_string = provision(name.clone(), Some(cache.clone())).unwrap();
        let by_reference = provision(parsed, Some(cache.clone())).unwrap();

        assert_eq!(by_string.path, by_str.path);
        assert_eq!(by_reference.path, by_str.path);
    }

    #[test]
    fn an_image_nothing_pulled_is_not_provisioned() {
        let scratch = Scratch::new("missing");
        let error = provision("test/missing:1", Some(scratch.cache())).unwrap_err();
        assert!(matches!(error, Error::NotPulled(_)), "{error}");
    }

    /// What is not a reference at all is said so rather than read as a name nothing
    /// pulled: the two are different mistakes and only one of them is fixed by a pull.
    #[test]
    fn what_is_not_a_reference_is_refused() {
        let scratch = Scratch::new("garbage");
        let error = provision("NOT A REFERENCE", Some(scratch.cache())).unwrap_err();
        assert!(matches!(error, Error::Reference(_)), "{error}");
    }
}
