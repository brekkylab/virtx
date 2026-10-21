//! An image's root filesystem: its layers made into one disk a guest can boot.
//!
//! # What it is
//!
//! One block device, read as one EROFS filesystem, made of the layer files where they
//! already are:
//!
//! ```text
//! <chain>.vmdk  =  [ <chain>.erofs │ layer │ layer │ … ]
//!                     │
//!                     └── every inode of the merged image, each file's data address
//!                         pointing into the layer its bytes are still sitting in
//! ```
//!
//! The guest mounts it and finds a filesystem. It is not an overlay and there is no stack
//! left in it: the layers were merged here -- later ones winning, whiteouts deleting,
//! opaque directories emptying -- and what the metadata image describes is the image as a
//! single tree.
//!
//! # What is not copied
//!
//! Any layer's contents. A root filesystem costs one metadata image and a few hundred
//! bytes of descriptor, whatever the layers weigh, and two images over one base share that
//! base rather than each holding a copy. That is why a layer is written the way it is: a
//! file keeps every byte of itself in its layer's data area, in whole blocks, so a merged
//! image can point at it.
//!
//! # What has to agree
//!
//! Where each layer starts. The metadata image addresses each layer it points into as
//! starting after itself and after every such layer before it; the descriptor puts it
//! after however many bytes those files hold. Were the two ever to come apart nothing
//! downstream would say so -- the guest would mount, read at the wrong offset and find
//! another layer's bytes -- so the descriptor is written from the very list the writer
//! answers with, and a layer that is not exactly as long as its superblock says is
//! refused as it is read.
//!
//! Nothing but [`Rootfs::create`] writes either file, so that this holds in one place.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::file::write_atomically;
use crate::filesystem::{self, Erofs, Tree, erofs};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("no layers make no filesystem to boot")]
    NoLayers,

    /// The files cannot be described as a disk: one cannot be named or addressed by a
    /// descriptor.
    #[error("cannot be described as a disk: {0}")]
    Descriptor(String),

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
    fn io(context: impl Into<String>, source: std::io::Error) -> Error {
        Error::Io {
            context: context.into(),
            source,
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// A root filesystem that has been made: two files beside each other, `<at>.erofs` and
/// `<at>.vmdk`, wherever whoever keeps it puts them -- a cache puts them under the layers'
/// ChainID (see [`Cache::rootfs_path`](crate::cache::Cache::rootfs_path)).
///
/// Only [`open`](Rootfs::open) and [`create`](Rootfs::create) make one, so one in hand is
/// one whose files were there.
#[derive(Clone, Debug)]
pub struct Rootfs {
    disk: PathBuf,
}

impl Rootfs {
    /// The root filesystem made at `at` before, if it is there: both files, because both
    /// are what a boot reads and either could have gone since. Neither alone is one.
    pub fn open(at: &Path) -> Option<Rootfs> {
        let (meta, disk) = files(at);
        (meta.is_file() && disk.is_file()).then_some(Rootfs { disk })
    }

    /// Make a root filesystem at `at` of the EROFS layers at `layers`, bottom first,
    /// replacing whatever was made there before.
    ///
    /// The disk names the layers by these paths, absolutely, so they have to stay where
    /// they are for as long as it is booted. Reads every layer's metadata and writes the
    /// merged image's, so it is blocking work: a caller on an async runtime moves it off the
    /// runtime's threads.
    ///
    /// ```no_run
    /// # fn demo(image: &virtx_uvm::image::Image) -> Result<(), Box<dyn std::error::Error>> {
    /// use virtx_uvm::cache::Cache;
    /// use virtx_uvm::rootfs::Rootfs;
    ///
    /// let cache = Cache::default();
    /// let at = cache.rootfs_path(&image.layers).ok_or("an image of no layers")?;
    /// let rootfs = match Rootfs::open(&at) {
    ///     Some(rootfs) => rootfs,
    ///     None => {
    ///         let layers: Vec<_> = image.layers.iter().map(|l| cache.layer_path(l)).collect();
    ///         Rootfs::create(&layers, &at)?
    ///     }
    /// };
    /// println!("boot {}", rootfs.disk().display());
    /// # Ok(())
    /// # }
    /// ```
    pub fn create(layers: &[impl AsRef<Path>], at: &Path) -> Result<Rootfs> {
        if layers.is_empty() {
            return Err(Error::NoLayers);
        }
        let (meta, disk) = files(at);

        // Every layer's tree, its files pointing into the layer they are in.
        let trees = layers
            .iter()
            .map(|layer| erofs::read_spanned(layer.as_ref()))
            .collect::<filesystem::Result<Vec<_>>>()?;

        // The writer answers with the layers the merged image points into, in the order it
        // addressed them as following it -- which is the disk's order, and so the
        // descriptor's. A layer nothing points into any more, every file of it replaced or
        // deleted above, is not in it.
        let merged = Tree::merge(trees);
        let behind = written(&meta, |file| Erofs::from_tree(&merged, file))??;

        // The descriptor last, so that a descriptor is never written before the metadata
        // image it fronts.
        let extents: Vec<&Path> = std::iter::once(meta.as_path())
            .chain(behind.iter().map(|layer| &**layer))
            .collect();
        written(&disk, |file| write_descriptor(&extents, file))??;

        Ok(Rootfs { disk })
    }

    /// The VMDK descriptor to give a VM as its root block device, read as `erofs`.
    ///
    /// Attached read-only. Nothing behind it is a file this owns alone -- the layers are
    /// shared with every image over them -- so a guest allowed to write to it would be
    /// writing into those.
    pub fn disk(&self) -> &Path {
        &self.disk
    }
}

/// The two files of the root filesystem at `at`: the merged metadata image, and the disk
/// it fronts. Appended to rather than an extension replaced, so a dot in `at` stays.
pub(crate) fn files(at: &Path) -> (PathBuf, PathBuf) {
    let with = |extension: &str| {
        let mut path = at.as_os_str().to_owned();
        path.push(".");
        path.push(extension);
        PathBuf::from(path)
    };
    (with("erofs"), with("vmdk"))
}

/// [`write_atomically`], its own failures said as this module's.
fn written<T, E>(
    path: &Path,
    write: impl FnOnce(&mut File) -> std::result::Result<T, E>,
) -> Result<std::result::Result<T, E>> {
    write_atomically(path, write).map_err(|e| Error::io(format!("writing {}", path.display()), e))
}

//--------------------------------------------------------------------------------------------------
// The descriptor
//--------------------------------------------------------------------------------------------------

// What `disk` is: a VMDK flat descriptor, forty lines of plain text naming a list of files
// and how many sectors to read of each, in order. That is what makes the metadata image
// and the layers behind it one block device with no copy of any of them.

/// One extent line addresses at most 2 GiB, in 512-byte sectors. A longer file is
/// several lines reading successive parts of it.
const MAX_EXTENT_SECTORS: u64 = 4_194_304;

/// The sector a descriptor counts in.
const SECTOR: u64 = 512;

/// Geometry nothing reads and every descriptor states.
const HEADS: u64 = 16;
const SECTORS_PER_TRACK: u64 = 63;

/// Write a descriptor on `out` that reads `extents`, in order, as one disk.
///
/// Every extent has to be a whole number of sectors, because a descriptor addresses
/// sectors and half of one has no spelling. An EROFS image is a whole number of 4 KiB
/// blocks, so this only fires on something that is not one.
fn write_descriptor(extents: &[&Path], out: &mut impl Write) -> Result<()> {
    if extents.is_empty() {
        return Err(Error::Descriptor("a disk needs at least one extent".into()));
    }

    let mut total = 0u64;
    let mut lines = Vec::new();

    for path in extents {
        let size = std::fs::metadata(path)
            .map_err(|e| Error::io(format!("reading {}", path.display()), e))?
            .len();
        if !size.is_multiple_of(SECTOR) {
            return Err(Error::Descriptor(format!(
                "{} is {size} bytes, which is not a whole number of {SECTOR}-byte sectors",
                path.display()
            )));
        }

        // Absolute, because a descriptor is read from wherever the VM happens to
        // stand, which is not where this was written.
        let absolute = std::fs::canonicalize(path)
            .map_err(|e| Error::io(format!("resolving {}", path.display()), e))?;
        // Not `Path::display`, which puts `U+FFFD` where a byte was not text and
        // hands back a name that opens some other file or none.
        let absolute = absolute.to_str().ok_or_else(|| {
            Error::Descriptor(format!(
                "{} is not valid UTF-8, which a descriptor cannot spell",
                absolute.display()
            ))
        })?;
        // The format puts one extent on one line and quotes the path, and offers no
        // escape for either delimiter. Refused rather than written, because what would
        // be written is a descriptor naming some other file: a quote ends the name
        // early, and a line break ends the extent and starts one nobody asked for.
        if absolute.contains(['"', '\n', '\r']) {
            return Err(Error::Descriptor(format!(
                "{absolute} has a quote or a line break in its name, \
                 which a descriptor cannot spell"
            )));
        }

        let sectors = size / SECTOR;
        let mut at = 0u64;
        while at < sectors {
            let chunk = (sectors - at).min(MAX_EXTENT_SECTORS);
            lines.push(format!("RW {chunk} FLAT \"{absolute}\" {at}"));
            at += chunk;
        }
        total += sectors;
    }

    let cylinders = total.div_ceil(HEADS * SECTORS_PER_TRACK);
    let mut text = String::new();
    text.push_str("# Disk DescriptorFile\n");
    text.push_str("version=1\n");
    text.push_str("CID=fffffffe\n");
    text.push_str("parentCID=ffffffff\n");
    text.push_str("createType=\"twoGbMaxExtentFlat\"\n\n");
    text.push_str("# Extent description\n");
    for line in &lines {
        text.push_str(line);
        text.push('\n');
    }
    text.push_str("\n# The Disk Data Base\n#DDB\n");
    text.push_str("ddb.virtualHWVersion = \"4\"\n");
    text.push_str(&format!("ddb.geometry.cylinders = \"{cylinders}\"\n"));
    text.push_str(&format!("ddb.geometry.heads = \"{HEADS}\"\n"));
    text.push_str(&format!("ddb.geometry.sectors = \"{SECTORS_PER_TRACK}\"\n"));
    text.push_str("ddb.adapterType = \"ide\"\n");

    out.write_all(text.as_bytes())
        .map_err(|e| Error::io("writing a disk descriptor", e))
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::filesystem::{Changeset, Data, Entry, MediaType, Meta};

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Scratch {
            let path = std::env::temp_dir().join(format!(
                "virtx-uvm-lib-rootfs-{name}-{}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Scratch(path)
        }

        /// Write `tree` here as an EROFS layer named `name`, and answer with where.
        fn layer(&self, name: &str, tree: &Tree) -> PathBuf {
            let path = self.0.join(format!("{name}.erofs"));
            Erofs::from_tree(tree, File::create(&path).unwrap()).unwrap();
            path
        }

        fn at(&self) -> PathBuf {
            self.0.join("rootfs")
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn file(bytes: &[u8]) -> Entry {
        Entry::File {
            meta: Meta {
                mode: 0o644,
                ..Meta::default()
            },
            data: Data::Buffer(bytes.to_vec()),
        }
    }

    fn base() -> Tree {
        let mut tree = Tree::new();
        tree.insert(b"etc/keep", file(b"kept")).unwrap();
        tree.insert(b"etc/replaced", file(b"old")).unwrap();
        tree.insert(b"etc/deleted", file(b"gone")).unwrap();
        tree.insert(b"var/cache/stale", file(b"stale")).unwrap();
        // Longer than a block, so that an address one block out reads the wrong bytes
        // rather than the right ones by luck.
        tree.insert(b"bin/tool", file(&[7u8; 5000])).unwrap();
        tree.link(b"bin/alias", b"bin/tool").unwrap();
        tree
    }

    fn upper() -> Tree {
        let mut tree = Tree::new();
        tree.insert(b"etc/replaced", file(b"new")).unwrap();
        tree.insert(b"etc/added", file(b"added")).unwrap();
        tree.insert(b"etc/deleted", Entry::Whiteout).unwrap();
        tree.opaque(b"var/cache").unwrap();
        tree.insert(b"var/cache/fresh", file(b"fresh")).unwrap();
        tree
    }

    /// The disk a descriptor describes, read the way a VM would: every extent it names, in
    /// order, end to end.
    fn read_disk(descriptor: &Path) -> Vec<u8> {
        let text = std::fs::read_to_string(descriptor).unwrap();
        let mut disk = Vec::new();
        for line in text.lines().filter(|line| line.starts_with("RW ")) {
            let mut fields = line.split('"');
            let sectors: u64 = fields
                .next()
                .unwrap()
                .split(' ')
                .nth(1)
                .unwrap()
                .parse()
                .unwrap();
            let path = fields.next().unwrap();
            let offset: u64 = fields.next().unwrap().trim().parse().unwrap();
            let bytes = std::fs::read(path).unwrap();
            disk.extend_from_slice(&bytes[(offset * 512) as usize..][..(sectors * 512) as usize]);
        }
        disk
    }

    /// The disk mounted, as far as a tree can say: every path, and what a file holds.
    fn mounted(disk: Vec<u8>) -> Vec<(String, Option<Vec<u8>>)> {
        let tree = Erofs::new(Cursor::new(disk)).to_tree().unwrap();
        tree.walk()
            .into_iter()
            .map(|(path, _, entry)| {
                let data = match entry {
                    Entry::File {
                        data: Data::Buffer(bytes),
                        ..
                    } => Some(bytes.clone()),
                    _ => None,
                };
                (String::from_utf8(path).unwrap(), data)
            })
            .collect()
    }

    #[test]
    fn a_disk_reads_as_the_layers_merged() {
        let scratch = Scratch::new("merged");
        let rootfs = Rootfs::create(
            &[scratch.layer("a", &base()), scratch.layer("b", &upper())],
            &scratch.at(),
        )
        .unwrap();

        let files = mounted(read_disk(rootfs.disk()));
        let get = |path: &str| {
            files
                .iter()
                .find(|(at, _)| at == path)
                .map(|(_, data)| data.clone())
        };

        assert_eq!(get("etc/keep"), Some(Some(b"kept".to_vec())));
        assert_eq!(
            get("etc/replaced"),
            Some(Some(b"new".to_vec())),
            "the upper did not win"
        );
        assert_eq!(get("etc/added"), Some(Some(b"added".to_vec())));
        assert_eq!(get("etc/deleted"), None, "the whiteout did not delete");
        assert_eq!(
            get("var/cache/stale"),
            None,
            "the opaque directory did not empty"
        );
        assert_eq!(get("var/cache/fresh"), Some(Some(b"fresh".to_vec())));
        assert_eq!(get("bin/tool"), Some(Some(vec![7u8; 5000])));
        assert_eq!(get("bin/alias"), Some(Some(vec![7u8; 5000])));
    }

    /// A merged image is a filesystem and not a changeset: nothing in it says "deleted" or
    /// "opaque", even from a lone layer that carried a marker over nothing.
    #[test]
    fn no_marker_survives_the_merge() {
        let mut only = base();
        only.insert(b"etc/orphan", Entry::Whiteout).unwrap();
        only.opaque(b"var").unwrap();

        let merged = Tree::merge([only]);
        for (path, _, entry) in merged.walk() {
            assert!(
                !matches!(
                    entry,
                    Entry::Whiteout | Entry::Directory { opaque: true, .. }
                ),
                "{} is still a marker",
                String::from_utf8_lossy(&path)
            );
        }
    }

    /// A merged tree points into its layers and holds none of their bytes, and is still a
    /// whole tree: any form can be written of it, the bytes read from where it points.
    #[test]
    fn a_merged_tree_is_a_whole_tree() {
        let scratch = Scratch::new("whole");
        let layers = [scratch.layer("a", &base()), scratch.layer("b", &upper())];
        let merged = Tree::merge(
            layers
                .iter()
                .map(|layer| erofs::read_spanned(layer.as_path()).unwrap()),
        );

        let mut archive = Vec::new();
        Changeset::from_tree(&merged, &mut archive).unwrap();
        let (read, _) = Changeset::new(&MediaType::ImageLayer, &archive[..])
            .unwrap()
            .to_tree()
            .unwrap();
        let tool = read
            .walk()
            .into_iter()
            .find(|(path, ..)| path == b"bin/tool")
            .map(|(_, _, entry)| entry.clone());
        assert!(matches!(
            tool,
            Some(Entry::File { data: Data::Buffer(bytes), .. }) if bytes == vec![7u8; 5000]
        ));
    }

    /// A layer whose every file a later one replaced or deleted is nothing the merged
    /// image points into, and so no part of the disk.
    #[test]
    fn a_layer_nothing_points_into_is_left_off_the_disk() {
        let scratch = Scratch::new("hidden");
        let mut hidden = Tree::new();
        hidden.insert(b"etc/replaced", file(b"older")).unwrap();
        let rootfs = Rootfs::create(
            &[
                scratch.layer("hidden", &hidden),
                scratch.layer("base", &base()),
            ],
            &scratch.at(),
        )
        .unwrap();

        let descriptor = std::fs::read_to_string(rootfs.disk()).unwrap();
        assert!(!descriptor.contains("hidden.erofs"), "{descriptor}");

        let files = mounted(read_disk(rootfs.disk()));
        assert!(files.contains(&("etc/replaced".to_string(), Some(b"old".to_vec()))));
    }

    #[test]
    fn a_hard_link_stays_one_inode() {
        let merged = Tree::merge([base(), upper()]);
        let walked = merged.walk();
        let inode = |path: &[u8]| walked.iter().find(|(at, ..)| at == path).unwrap().1;
        assert_eq!(inode(b"bin/tool"), inode(b"bin/alias"));
        assert_ne!(inode(b"bin/tool"), inode(b"etc/keep"));
    }

    /// A layer that is longer than its superblock says would put every layer behind it
    /// somewhere the metadata does not point.
    #[test]
    fn a_layer_of_the_wrong_length_is_refused() {
        let scratch = Scratch::new("length");
        let layers = [scratch.layer("a", &base()), scratch.layer("b", &upper())];
        let grown = std::fs::OpenOptions::new()
            .append(true)
            .open(&layers[0])
            .unwrap();
        grown
            .set_len(grown.metadata().unwrap().len() + 4096)
            .unwrap();

        assert!(matches!(
            Rootfs::create(&layers, &scratch.at()),
            Err(Error::Filesystem(_))
        ));
        assert!(
            Rootfs::open(&scratch.at()).is_none(),
            "a root filesystem was left behind"
        );
    }

    /// Half of one is none: a boot reads both files.
    #[test]
    fn a_rootfs_missing_either_file_does_not_open() {
        let scratch = Scratch::new("half");
        Rootfs::create(&[scratch.layer("a", &base())], &scratch.at()).unwrap();
        assert!(Rootfs::open(&scratch.at()).is_some());

        std::fs::remove_file(scratch.0.join("rootfs.erofs")).unwrap();
        assert!(Rootfs::open(&scratch.at()).is_none());
    }

    #[test]
    fn no_layers_make_none() {
        let scratch = Scratch::new("empty");
        assert!(matches!(
            Rootfs::create(&[] as &[PathBuf], &scratch.at()),
            Err(Error::NoLayers)
        ));
    }
}

#[cfg(test)]
mod descriptor_tests {
    use std::path::PathBuf;

    use super::*;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Scratch {
            let path =
                std::env::temp_dir().join(format!("virtx-uvm-vmdk-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Scratch(path)
        }

        fn extent(&self, name: &str, bytes: u64) -> PathBuf {
            let path = self.0.join(name);
            let file = std::fs::File::create(&path).unwrap();
            file.set_len(bytes).unwrap();
            path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn descriptor(extents: &[&Path]) -> Result<String> {
        let mut out = Vec::new();
        write_descriptor(extents, &mut out)?;
        Ok(String::from_utf8(out).unwrap())
    }

    fn extent_lines(text: &str) -> Vec<&str> {
        text.lines()
            .filter(|line| line.starts_with("RW "))
            .collect()
    }

    #[test]
    fn a_descriptor_names_every_extent_in_order() {
        let scratch = Scratch::new("order");
        let (a, b) = (scratch.extent("a.bin", 4096), scratch.extent("b.bin", 8192));

        let text = descriptor(&[&a, &b]).unwrap();
        let lines = extent_lines(&text);

        assert!(text.contains("createType=\"twoGbMaxExtentFlat\""), "{text}");
        assert_eq!(lines.len(), 2, "{text}");
        assert!(lines[0].starts_with("RW 8 FLAT "), "{}", lines[0]);
        assert!(lines[0].contains("a.bin"), "{}", lines[0]);
        assert!(lines[1].starts_with("RW 16 FLAT "), "{}", lines[1]);
        // 24 sectors over 16 heads * 63 sectors, rounded up.
        assert!(text.contains("ddb.geometry.cylinders = \"1\""), "{text}");
    }

    #[test]
    fn an_extent_is_named_absolutely() {
        let scratch = Scratch::new("absolute");
        let only = scratch.extent("a.bin", 512);

        let text = descriptor(&[&only]).unwrap();
        let named = text
            .lines()
            .find(|line| line.starts_with("RW "))
            .and_then(|line| line.split('"').nth(1))
            .expect("an extent line with a path");
        assert!(
            Path::new(named).is_absolute(),
            "the extent is named {named:?}, which a VM standing elsewhere cannot open"
        );
    }

    /// Over 2 GiB is several lines reading successive parts of one file, and the
    /// offsets are what say which part.
    #[test]
    fn an_extent_over_two_gibibytes_is_split() {
        let scratch = Scratch::new("big");
        let big = scratch.extent("big.bin", 3 << 30);

        let lines = descriptor(&[&big]).unwrap();
        let lines = extent_lines(&lines);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines[0].starts_with("RW 4194304 FLAT "), "{}", lines[0]);
        assert!(lines[0].ends_with(" 0"), "{}", lines[0]);
        assert!(lines[1].starts_with("RW 2097152 FLAT "), "{}", lines[1]);
        assert!(lines[1].ends_with(" 4194304"), "{}", lines[1]);
    }

    #[test]
    fn an_extent_that_is_not_whole_sectors_is_refused() {
        let scratch = Scratch::new("ragged");
        let ragged = scratch.extent("ragged.bin", 1000);
        assert!(matches!(descriptor(&[&ragged]), Err(Error::Descriptor(_))));
    }

    /// A line break is the quote's twin: one ends the name early, the other ends the
    /// extent and begins one nobody asked for.
    ///
    /// Unix only, because the name this refuses is one Windows has no way to hand it:
    /// a quote and a line break are both illegal in an NTFS name, so the filesystem
    /// turns the file away before a descriptor could ever spell it.
    #[cfg(unix)]
    #[test]
    fn a_name_that_would_forge_an_extent_line_is_refused() {
        let scratch = Scratch::new("forged");
        for forged in ["a\nRW 8 FLAT \"passwd\" 0", "a\rb", "a\"b"] {
            let path = scratch.extent(forged, 512);
            assert!(
                matches!(descriptor(&[&path]), Err(Error::Descriptor(_))),
                "{forged:?} was accepted"
            );
        }
    }

    #[test]
    fn a_disk_of_nothing_is_refused() {
        assert!(matches!(descriptor(&[]), Err(Error::Descriptor(_))));
    }
}
