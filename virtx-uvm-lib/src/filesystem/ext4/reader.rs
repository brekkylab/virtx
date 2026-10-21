//! Reading an ext4 image as a [`Tree`].

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Seek, SeekFrom};

use ext4::{Checksums, Enhanced, Options, SuperBlock};

use crate::filesystem::tree::{Data, Entry, Meta, Tree};
use crate::filesystem::{Error, Form, Result};

/// How deep a directory may nest before this stops following it. A filesystem has no
/// directory inside itself, but an image that is not one can say it does.
const MAX_DEPTH: usize = 256;

/// What overlayfs marks a directory with when it hides everything below it.
const OPAQUE: &str = "trusted.overlay.opaque";

/// Everything overlayfs writes about its own mount.
const OVERLAY: &str = "trusted.overlay.";

/// Where ext4 keeps a file's inline data, which is the filesystem's own business and not an
/// attribute anybody set. A kernel never lists it; the formatter here writes an empty one
/// beside every attribute it is given.
const INLINE_DATA: &str = "system.data";

/// Read `image` as a tree, file contents and all.
pub fn read(image: impl Read + Seek) -> Result<Tree> {
    // `Checksums::Enabled` and not the default `Required`: every checksum that is there is
    // verified, and a filesystem that carries none is still read -- which is every one
    // the formatter here writes.
    let options = Options {
        checksums: Checksums::Enabled,
    };
    let fs = SuperBlock::new_with_options(Positioned(RefCell::new(image)), &options)
        .map_err(|e| failed("reading", e))?;
    let root = fs.root().map_err(|e| failed("reading the root of", e))?;

    let mut walk = Walk {
        fs: &fs,
        tree: Tree::new(),
        files: HashMap::new(),
    };
    walk.directory(&root, Vec::new(), 0)?;
    Ok(walk.tree)
}

/// The tree being read into, and what the walk has to remember on the way.
struct Walk<'a, R: positioned_io::ReadAt> {
    fs: &'a SuperBlock<R>,
    tree: Tree,
    /// The first path each file with more than one name was read at, so that the names
    /// after it are links to it and not copies of it.
    files: HashMap<u32, Vec<u8>>,
}

impl<R: positioned_io::ReadAt> Walk<'_, R> {
    /// The directory `inode` at `path`, and everything under it.
    fn directory(&mut self, inode: &ext4::Inode, path: Vec<u8>, depth: usize) -> Result<()> {
        if depth > MAX_DEPTH {
            return Err(Error::malformed(
                Form::Ext4,
                format!("directories nest more than {MAX_DEPTH} deep"),
            ));
        }
        let shown = show(&path);
        let mut listing = match self
            .fs
            .enhance(inode)
            .map_err(|e| failed(&format!("reading {shown} on"), e))?
        {
            Enhanced::Directory(listing) => listing,
            other => {
                return Err(Error::malformed(
                    Form::Ext4,
                    format!("{shown} is {other:?}, and a directory was expected"),
                ));
            }
        };
        let (meta, opaque) = described(inode);
        self.tree
            .insert(&path, Entry::Directory { meta, opaque })
            .map_err(|e| e.reading(Form::Ext4))?;

        // By name, so that one filesystem reads as one tree whatever order its directory
        // blocks happened to hold their entries in.
        listing.sort_by(|a, b| a.name.cmp(&b.name));

        for listed in listing {
            if listed.name == "." || listed.name == ".." {
                continue;
            }
            let mut path = path.clone();
            if !path.is_empty() {
                path.push(b'/');
            }
            path.extend_from_slice(listed.name.as_bytes());
            let reading = |e| failed(&format!("reading {} on", show(&path)), e);

            let inode = self.fs.load_inode(listed.inode).map_err(reading)?;
            let enhanced = self.fs.enhance(&inode).map_err(reading)?;
            let (meta, _) = described(&inode);

            let entry = match enhanced {
                // A deletion, and not a device.
                Enhanced::CharacterDevice(0, 0) => Entry::Whiteout,
                Enhanced::Directory(_) => {
                    self.directory(&inode, path, depth + 1)?;
                    continue;
                }
                Enhanced::RegularFile => {
                    if inode.stat.link_count > 1 {
                        if let Some(first) = self.files.get(&listed.inode) {
                            self.tree
                                .link(&path, first)
                                .map_err(|e| e.reading(Form::Ext4))?;
                            continue;
                        }
                        self.files.insert(listed.inode, path.clone());
                    }
                    let mut data = Vec::new();
                    self.fs
                        .open(&inode)
                        .map_err(reading)?
                        .read_to_end(&mut data)
                        .map_err(|e| Error::io(format!("reading {}", show(&path)), e))?;
                    Entry::File {
                        meta,
                        data: Data::Buffer(data),
                    }
                }
                Enhanced::SymbolicLink(target) => Entry::Symlink {
                    meta,
                    target: target.into_bytes(),
                },
                Enhanced::CharacterDevice(major, minor) => Entry::Device {
                    meta,
                    block: false,
                    major: u32::from(major),
                    minor,
                },
                Enhanced::BlockDevice(major, minor) => Entry::Device {
                    meta,
                    block: true,
                    major: u32::from(major),
                    minor,
                },
                Enhanced::Fifo => Entry::Fifo(meta),
                // A name a running process bound, which means nothing once that process is
                // gone. A tree has no entry for one.
                Enhanced::Socket => continue,
            };
            self.tree
                .insert(&path, entry)
                .map_err(|e| e.reading(Form::Ext4))?;
        }

        Ok(())
    }
}

/// What an inode says about itself that a tree keeps, and whether it is opaque. Every
/// attribute under `trusted.overlay.` is left out of what is kept, and so is the one ext4
/// keeps inline data in.
fn described(inode: &ext4::Inode) -> (Meta, bool) {
    let stat = &inode.stat;
    let xattrs: BTreeMap<Vec<u8>, Vec<u8>> = stat
        .xattrs
        .iter()
        .filter(|(name, _)| !name.starts_with(OVERLAY) && *name != INLINE_DATA)
        .map(|(name, value)| (name.clone().into_bytes(), value.clone()))
        .collect();
    let meta = Meta {
        mode: stat.file_mode & 0o7777,
        uid: stat.uid,
        gid: stat.gid,
        mtime: i64::from(stat.mtime.epoch_secs),
        xattrs,
    };
    (
        meta,
        stat.xattrs.get(OPAQUE).is_some_and(|value| value == b"y"),
    )
}

/// A reader, as the ext4 crate reads: at a position, from a shared reference. Each read
/// seeks first, so nothing relies on where the last one left off.
struct Positioned<R>(RefCell<R>);

impl<R: Read + Seek> positioned_io::ReadAt for Positioned<R> {
    fn read_at(&self, pos: u64, buf: &mut [u8]) -> std::io::Result<usize> {
        let mut image = self.0.borrow_mut();
        image.seek(SeekFrom::Start(pos))?;
        image.read(buf)
    }
}

/// Something the filesystem could not be asked. The crate answers with an `anyhow::Error`,
/// whose whole chain is said: what went wrong in an extent tree is several sentences deep.
fn failed(doing: &str, e: impl std::fmt::Display) -> Error {
    Error::malformed(Form::Ext4, format!("{doing} the image: {e:#}"))
}

fn show(path: &[u8]) -> String {
    if path.is_empty() {
        "/".into()
    } else {
        String::from_utf8_lossy(path).into_owned()
    }
}
