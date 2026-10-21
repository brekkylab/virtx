//! Reading an EROFS image back out as a [`Tree`].
//!
//! What this reads is what [`write`](super::writer::write) writes and not the format at
//! large: extended inodes, uncompressed data, no inlined tails. Every layer under the
//! cache is written by that, so an image saying anything else did not come from here,
//! and saying so is better than reading it as something it is not.
//!
//! The overlayfs spelling the writer puts whiteouts and opaque directories in is read
//! back out of it here, so a tree that went through an image comes back as it went in.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::Arc;

use super::format::*;
use crate::filesystem::tree::{Data, Entry, Meta, Tree};
use crate::filesystem::{Error, Form, Result};

/// How deep a directory may nest before this stops following it.
///
/// A tree written from a layer archive is nowhere near this deep. What the cap is for is
/// an image that is not a tree at all -- a dirent pointing back at an ancestor makes a
/// cycle. The directories already open catch the plain cycles; this catches the rest.
const MAX_DEPTH: usize = 256;

/// The xattr the writer marks an opaque directory with.
const OPAQUE: &[u8] = b"trusted.overlay.opaque";

/// Read `image` as a tree, file contents and all.
pub fn read(image: impl Read + Seek) -> Result<Tree> {
    let mut reader = Reader::open(image)?;
    walk(&mut reader, &mut |reader, inode| {
        reader.contents(inode).map(Data::Buffer)
    })
}

/// Read the image at `file` as a tree whose files point into it rather than holding
/// their bytes.
///
/// The image has to be exactly as long as its superblock says. What is made of a tree like
/// this is a disk with the image somewhere along it, and everything after the image on
/// that disk is addressed as if it were.
pub fn read_spanned(file: Arc<Path>) -> Result<Tree> {
    let opened =
        File::open(&file).map_err(|e| Error::io(format!("opening {}", file.display()), e))?;
    let mut reader = Reader::open(opened)?;

    let len = reader
        .image
        .seek(SeekFrom::End(0))
        .map_err(|e| Error::io(format!("reading {}", file.display()), e))?;
    if len != reader.bytes {
        return Err(malformed(format!(
            "{} is {len} bytes and its superblock accounts for {}",
            file.display(),
            reader.bytes
        )));
    }

    walk(&mut reader, &mut |reader, inode| {
        // Asked here because nothing reads the bytes to ask it later: a file that runs
        // past the image would run into whatever is after it on a disk.
        let offset = u64::from(inode.u) * BLOCK_SIZE;
        if offset.saturating_add(inode.size) > reader.bytes {
            return Err(malformed(format!(
                "{} bytes at {offset} run past the end of the image",
                inode.size
            )));
        }
        Ok(Data::Span {
            file: file.clone(),
            offset,
            len: inode.size,
        })
    })
}

/// The tree under the root of `reader`, each regular file's data being what `data` makes
/// of its inode.
fn walk<R: Read + Seek>(
    reader: &mut Reader<R>,
    data: &mut impl FnMut(&mut Reader<R>, &Inode) -> Result<Data>,
) -> Result<Tree> {
    let nid = reader.root;
    let root = reader.inode(nid)?;
    if root.mode & S_IFMT != S_IFDIR {
        return Err(malformed(format!(
            "the root inode is mode {:o}, which is not a directory",
            root.mode
        )));
    }

    let mut walk = Walk {
        tree: Tree::new(),
        files: HashMap::new(),
        open: HashSet::new(),
    };
    walk.directory(reader, data, nid, root, Vec::new(), 0)?;
    Ok(walk.tree)
}

/// An image, and where the things in it are.
struct Reader<R> {
    image: R,
    /// Byte offset of the metadata area, which every nid counts slots from.
    meta: u64,
    root: u64,
    /// How long the image says it is, which nothing it points at may run past.
    bytes: u64,
}

/// One inode, as far as this reads them.
struct Inode {
    mode: u16,
    size: u64,
    /// `i_u`: a data address for most things, a device number for a device.
    u: u32,
    meta: Meta,
}

impl<R: Read + Seek> Reader<R> {
    fn open(mut image: R) -> Result<Reader<R>> {
        let mut sb = [0u8; SUPER_SIZE];
        image
            .seek(SeekFrom::Start(SUPER_OFFSET))
            .map_err(|e| Error::io("reading an EROFS image", e))?;
        image
            .read_exact(&mut sb)
            .map_err(|_| malformed("the file is too short to hold a superblock".into()))?;

        if u32::from_le_bytes(sb[0x00..0x04].try_into().unwrap()) != SUPER_MAGIC {
            return Err(malformed("not an EROFS image".into()));
        }
        if sb[0x0C] != BLOCK_BITS {
            return Err(unsupported(format!(
                "blocks of 2^{} bytes, and this reads 2^{BLOCK_BITS}",
                sb[0x0C]
            )));
        }

        let blocks = u32::from_le_bytes(sb[0x24..0x28].try_into().unwrap());
        let meta = u64::from(u32::from_le_bytes(sb[0x28..0x2C].try_into().unwrap()));
        let root = u64::from(u16::from_le_bytes(sb[0x0E..0x10].try_into().unwrap()));

        Ok(Reader {
            image,
            meta: meta * BLOCK_SIZE,
            root,
            bytes: u64::from(blocks) * BLOCK_SIZE,
        })
    }

    /// The inode named `nid`, with its inline xattrs.
    fn inode(&mut self, nid: u64) -> Result<Inode> {
        let at = self.meta + nid * SLOT_SIZE;
        let raw: [u8; INODE_SIZE as usize] = self.at(at, INODE_SIZE)?.try_into().unwrap();

        let format = u16::from_le_bytes(raw[0..2].try_into().unwrap());
        if format & 1 != INODE_VERSION_EXTENDED {
            return Err(unsupported(format!(
                "inode {nid} is compact, and this reads the 64-byte form"
            )));
        }

        let mode = u16::from_le_bytes(raw[4..6].try_into().unwrap());
        let size = u64::from_le_bytes(raw[8..16].try_into().unwrap());

        let layout = (format >> 1) & 0b111;
        if layout != LAYOUT_FLAT_PLAIN && size > 0 {
            return Err(unsupported(format!(
                "inode {nid} has data layout {layout}, and this reads the plain one"
            )));
        }

        let icount = u16::from_le_bytes(raw[2..4].try_into().unwrap());
        let xattrs = self.xattrs(at + INODE_SIZE, icount)?;

        Ok(Inode {
            mode,
            size,
            u: u32::from_le_bytes(raw[16..20].try_into().unwrap()),
            meta: Meta {
                mode: mode & 0o7777,
                uid: u32::from_le_bytes(raw[24..28].try_into().unwrap()),
                gid: u32::from_le_bytes(raw[28..32].try_into().unwrap()),
                mtime: i64::from_le_bytes(raw[32..40].try_into().unwrap()),
                xattrs,
            },
        })
    }

    /// The xattrs sitting behind an inode, `icount` describing how many bytes of them
    /// there are. A name whose prefix index the format does not define is dropped, as the
    /// writer drops a name it cannot store.
    fn xattrs(
        &mut self,
        at: u64,
        icount: u16,
    ) -> Result<std::collections::BTreeMap<Vec<u8>, Vec<u8>>> {
        let mut xattrs = std::collections::BTreeMap::new();
        if icount == 0 {
            return Ok(xattrs);
        }

        let body = self.at(at, XATTR_HEADER_SIZE + 4 * u64::from(icount - 1))?;
        let mut cursor = XATTR_HEADER_SIZE as usize;
        while cursor + 4 <= body.len() {
            let name_len = body[cursor] as usize;
            let index = body[cursor + 1];
            let value_len =
                u16::from_le_bytes(body[cursor + 2..cursor + 4].try_into().unwrap()) as usize;

            let end = cursor + 4 + name_len + value_len;
            if end > body.len() {
                return Err(malformed(
                    "an xattr runs past the end of the inode holding it".into(),
                ));
            }
            if let Some(name) = xattr_name(index, &body[cursor + 4..cursor + 4 + name_len]) {
                xattrs.insert(name, body[cursor + 4 + name_len..end].to_vec());
            }

            cursor += xattr_align((4 + name_len + value_len) as u64) as usize;
        }

        Ok(xattrs)
    }

    /// An inode's contents: whole blocks of the data area, from the one `i_u` names.
    fn contents(&mut self, inode: &Inode) -> Result<Vec<u8>> {
        self.at(u64::from(inode.u) * BLOCK_SIZE, inode.size)
    }

    fn at(&mut self, offset: u64, len: u64) -> Result<Vec<u8>> {
        // Asked before anything is allocated, so a size an image lies about cannot ask
        // for more memory than the image is long.
        if offset.saturating_add(len) > self.bytes {
            return Err(malformed(format!(
                "{len} bytes at {offset} run past the end of the image"
            )));
        }
        let mut bytes = vec![0u8; len as usize];
        self.image
            .seek(SeekFrom::Start(offset))
            .map_err(|e| Error::io("reading an EROFS image", e))?;
        self.image.read_exact(&mut bytes).map_err(|_| {
            malformed(format!(
                "the image ends before the {len} bytes it says are at {offset}"
            ))
        })?;
        Ok(bytes)
    }

    /// A directory's entries, as `(name, nid)`, `.` and `..` left out.
    ///
    /// A block holds its dirents at the front and its names packed behind them, and
    /// where the names start is how many dirents there are: the first dirent's name
    /// offset is the end of the table. The last name in a block runs to the block's end,
    /// padded with the zeros a name cannot contain.
    fn entries(&mut self, nid: u64, inode: &Inode) -> Result<Vec<(Vec<u8>, u64)>> {
        let data = self.contents(inode)?;
        let wrong = |what: &str| malformed(format!("directory {nid}: {what}"));

        let mut entries = Vec::new();
        let mut at = 0;
        while at < data.len() {
            let block = &data[at..(at + BLOCK_SIZE as usize).min(data.len())];
            if block.len() < DIRENT_SIZE {
                return Err(wrong("a block too short to hold one entry"));
            }

            let table = u16::from_le_bytes(block[8..10].try_into().unwrap()) as usize;
            if table < DIRENT_SIZE || table > block.len() || !table.is_multiple_of(DIRENT_SIZE) {
                return Err(wrong("a name that does not start after the entries"));
            }
            let count = table / DIRENT_SIZE;

            for slot in 0..count {
                let dirent = &block[slot * DIRENT_SIZE..];
                let name_at = u16::from_le_bytes(dirent[8..10].try_into().unwrap()) as usize;
                let name_end = if slot + 1 == count {
                    block.len()
                } else {
                    u16::from_le_bytes(
                        block[(slot + 1) * DIRENT_SIZE + 8..][..2]
                            .try_into()
                            .unwrap(),
                    ) as usize
                };
                if name_at > name_end || name_end > block.len() {
                    return Err(wrong("a name outside the block holding it"));
                }

                let mut name = block[name_at..name_end].to_vec();
                while name.last() == Some(&0) {
                    name.pop();
                }
                if name.is_empty() {
                    return Err(wrong("an entry with no name"));
                }
                if name != b"." && name != b".." {
                    entries.push((name, u64::from_le_bytes(dirent[0..8].try_into().unwrap())));
                }
            }

            at += BLOCK_SIZE as usize;
        }

        Ok(entries)
    }
}

/// The tree being read into, and what the walk has to remember on the way.
struct Walk {
    tree: Tree,
    /// The first path each file's nid was read at. A nid reached by a second name is one
    /// inode under two names, which is what a hard link is.
    files: HashMap<u64, Vec<u8>>,
    /// The directories being read, to refuse one that is inside itself.
    open: HashSet<u64>,
}

impl Walk {
    /// The directory at `nid`, and everything under it.
    fn directory<R: Read + Seek>(
        &mut self,
        reader: &mut Reader<R>,
        data: &mut impl FnMut(&mut Reader<R>, &Inode) -> Result<Data>,
        nid: u64,
        inode: Inode,
        path: Vec<u8>,
        depth: usize,
    ) -> Result<()> {
        if depth > MAX_DEPTH {
            return Err(malformed(format!(
                "directories nest more than {MAX_DEPTH} deep"
            )));
        }
        // `.` and `..` are left out of the entries, but any other way back up would go
        // round for ever. Refusing a directory already on the way in catches it.
        if !self.open.insert(nid) {
            return Err(malformed(format!("directory {nid} is inside itself")));
        }

        let entries = reader.entries(nid, &inode)?;
        let mut meta = inode.meta;
        let opaque = meta.xattrs.get(OPAQUE).is_some_and(|value| value == b"y");
        if opaque {
            meta.xattrs.remove(OPAQUE);
        }
        self.tree
            .insert(&path, Entry::Directory { meta, opaque })
            .map_err(|e| e.reading(Form::Erofs))?;

        for (name, child) in entries {
            let mut path = path.clone();
            if !path.is_empty() {
                path.push(b'/');
            }
            path.extend_from_slice(&name);

            let inode = reader.inode(child)?;
            let entry = match inode.mode & S_IFMT {
                S_IFDIR => {
                    self.directory(reader, data, child, inode, path, depth + 1)?;
                    continue;
                }
                S_IFREG => {
                    if let Some(first) = self.files.get(&child) {
                        self.tree
                            .link(&path, first)
                            .map_err(|e| e.reading(Form::Erofs))?;
                        continue;
                    }
                    self.files.insert(child, path.clone());
                    Entry::File {
                        data: data(reader, &inode)?,
                        meta: inode.meta,
                    }
                }
                S_IFLNK => Entry::Symlink {
                    target: reader.contents(&inode)?,
                    meta: inode.meta,
                },
                kind @ (S_IFCHR | S_IFBLK) => match decode_dev(inode.u) {
                    // What the writer spells a whiteout as, and what overlayfs reads one
                    // as.
                    (0, 0) if kind == S_IFCHR => Entry::Whiteout,
                    (major, minor) => Entry::Device {
                        meta: inode.meta,
                        block: kind == S_IFBLK,
                        major,
                        minor,
                    },
                },
                S_IFIFO => Entry::Fifo(inode.meta),
                other => {
                    return Err(unsupported(format!(
                        "{} is mode {other:o}, which a tree has no entry for",
                        String::from_utf8_lossy(&name)
                    )));
                }
            };
            self.tree
                .insert(&path, entry)
                .map_err(|e| e.reading(Form::Erofs))?;
        }

        self.open.remove(&nid);
        Ok(())
    }
}

fn malformed(what: String) -> Error {
    Error::malformed(Form::Erofs, what)
}

fn unsupported(what: String) -> Error {
    Error::unsupported(Form::Erofs, what)
}
