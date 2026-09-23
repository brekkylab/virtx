//! Reading an EROFS image back out as a [`Tree`].
//!
//! The way round nothing else in this crate goes, and the reason a stack of layers
//! needs no notes kept beside it. A layer image already says where every one of its
//! files is: `i_u` on a `FLAT_PLAIN` inode *is* the block its data starts at, and
//! `i_size` is how much of it there is. So the tree that comes back here carries
//! [`Extent`]s which are byte offsets into the image that was read -- which is
//! exactly what a merged metadata image has to point at, one layer's worth at a time.
//!
//! What this reads is what [`write`](super::write) writes and not the format at
//! large: extended inodes, uncompressed data, no inlined tails. Every layer under the
//! cache is written by that, so an image saying anything else did not come from here,
//! and saying so is better than reading it as something it is not.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};

use super::format::*;
use super::{Error, Layout, Result};
use crate::image::tree::{Directory, Extent, InodeId, Meta, Node, Tree, Xattr};
use crate::image::tree::{S_IFBLK, S_IFCHR, S_IFDIR, S_IFIFO, S_IFLNK, S_IFMT, S_IFREG};

/// How deep a directory may nest before this stops following it.
///
/// A tree written from a layer archive is nowhere near this deep. What the cap is for
/// is an image that is not a tree at all -- a dirent pointing back at an ancestor
/// makes a cycle, and the walk below would follow it until the stack ran out. The
/// nids already visited catch the plain cycles; this catches the rest.
const MAX_DEPTH: usize = 256;

/// Read `image` as a tree, and say what the image turned out to be.
///
/// Every regular file's [`Extent`] is where its bytes are **in this image**, so a
/// caller stacking several has to move each tree onto the disk before merging them.
/// No file's contents are read: what this costs follows how many entries the image
/// has, not how large they are.
pub fn read<R: Read + Seek>(image: &mut R) -> Result<(Tree, Layout)> {
    let mut reader = Reader::open(image)?;

    let root = reader.inode(reader.root)?;
    if root.mode & S_IFMT != S_IFDIR {
        return Err(Error::Malformed(format!(
            "the root inode is mode {:o}, which is not a directory",
            root.mode
        )));
    }

    let mut tree = Tree::new();
    let mut ids = HashMap::new();
    let mut open = HashSet::new();
    let root = reader.directory(reader.root, &root, &mut tree, &mut ids, &mut open, 0)?;
    tree.root = root;

    let layout = Layout {
        blocks: reader.blocks,
        bytes: u64::from(reader.blocks) * BLOCK_SIZE,
        inodes: reader.inodes,
    };
    Ok((tree, layout))
}

/// An image, and where the things in it are.
struct Reader<'a, R> {
    image: &'a mut R,
    /// Byte offset of the metadata area, which every nid counts slots from.
    meta: u64,
    root: u64,
    blocks: u32,
    inodes: u64,
}

/// One inode, as far as this reads them.
struct Inode {
    mode: u16,
    size: u64,
    /// `i_u`: a data address for most things, a device number for a device.
    u: u32,
    meta: Meta,
}

impl<'a, R: Read + Seek> Reader<'a, R> {
    fn open(image: &'a mut R) -> Result<Reader<'a, R>> {
        let mut sb = [0u8; SUPER_SIZE];
        image.seek(SeekFrom::Start(SUPER_OFFSET))?;
        image
            .read_exact(&mut sb)
            .map_err(|_| Error::Malformed("the file is too short to hold a superblock".into()))?;

        if u32::from_le_bytes(sb[0x00..0x04].try_into().unwrap()) != SUPER_MAGIC {
            return Err(Error::Malformed("not an EROFS image".into()));
        }
        if sb[0x0C] != BLOCK_BITS {
            return Err(Error::Unsupported(format!(
                "blocks of 2^{} bytes, and this reads 2^{BLOCK_BITS}",
                sb[0x0C]
            )));
        }

        let inodes = u64::from_le_bytes(sb[0x10..0x18].try_into().unwrap());
        let blocks = u32::from_le_bytes(sb[0x24..0x28].try_into().unwrap());
        let meta = u64::from(u32::from_le_bytes(sb[0x28..0x2C].try_into().unwrap()));
        let root = u64::from(u16::from_le_bytes(sb[0x0E..0x10].try_into().unwrap()));

        Ok(Reader {
            image,
            meta: meta * BLOCK_SIZE,
            root,
            blocks,
            inodes,
        })
    }

    /// The inode named `nid`, with its inline xattrs.
    fn inode(&mut self, nid: u64) -> Result<Inode> {
        let at = self.meta + nid * SLOT_SIZE;
        let raw: [u8; INODE_SIZE as usize] = self.at(at, INODE_SIZE)?.try_into().unwrap();

        let format = u16::from_le_bytes(raw[0..2].try_into().unwrap());
        if format & 1 != INODE_VERSION_EXTENDED {
            return Err(Error::Unsupported(format!(
                "inode {nid} is compact, and this reads the 64-byte form"
            )));
        }

        let mode = u16::from_le_bytes(raw[4..6].try_into().unwrap());
        let size = u64::from_le_bytes(raw[8..16].try_into().unwrap());

        // A tail behind the inode is bytes in the metadata area, and a merge replaces
        // the metadata area. Nothing this writes lays a file out that way, so an image
        // that does is one this cannot hand on whole.
        let layout = (format >> 1) & 0b111;
        if layout != LAYOUT_FLAT_PLAIN && size > 0 {
            return Err(Error::Unsupported(format!(
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
    /// there are.
    ///
    /// A name whose prefix index the format does not define is dropped rather than
    /// refused, which is the same answer [`encode_xattrs`](super::write) gives to a
    /// name it cannot store: an image is still worth reading for the rest of what is
    /// in it.
    fn xattrs(&mut self, at: u64, icount: u16) -> Result<Vec<Xattr>> {
        if icount == 0 {
            return Ok(Vec::new());
        }

        let size = XATTR_HEADER_SIZE + 4 * u64::from(icount - 1);
        let body = self.at(at, size)?;

        let mut xattrs = Vec::new();
        let mut cursor = XATTR_HEADER_SIZE as usize;
        while cursor + 4 <= body.len() {
            let name_len = body[cursor] as usize;
            let index = body[cursor + 1];
            let value_len =
                u16::from_le_bytes(body[cursor + 2..cursor + 4].try_into().unwrap()) as usize;

            let end = cursor + 4 + name_len + value_len;
            if end > body.len() {
                return Err(Error::Malformed(
                    "an xattr runs past the end of the inode holding it".into(),
                ));
            }

            if let Some(name) = xattr_name(index, &body[cursor + 4..cursor + 4 + name_len]) {
                xattrs.push(Xattr {
                    name,
                    value: body[cursor + 4 + name_len..end].to_vec(),
                });
            }

            cursor += xattr_align((4 + name_len + value_len) as u64) as usize;
        }

        Ok(xattrs)
    }

    /// One inode's contents, which for everything this reads is whole blocks of the
    /// data area. Only asked for directories and symlinks, both of which are small.
    fn contents(&mut self, inode: &Inode) -> Result<Vec<u8>> {
        self.at(u64::from(inode.u) * BLOCK_SIZE, inode.size)
    }

    fn at(&mut self, offset: u64, len: u64) -> Result<Vec<u8>> {
        let mut bytes = vec![0u8; len as usize];
        self.image.seek(SeekFrom::Start(offset))?;
        self.image.read_exact(&mut bytes).map_err(|_| {
            Error::Malformed(format!(
                "the image ends before the {len} bytes it says are at {offset}"
            ))
        })?;
        Ok(bytes)
    }

    /// The directory at `nid`, and everything under it.
    fn directory(
        &mut self,
        nid: u64,
        inode: &Inode,
        tree: &mut Tree,
        ids: &mut HashMap<u64, InodeId>,
        open: &mut HashSet<u64>,
        depth: usize,
    ) -> Result<Directory> {
        if depth > MAX_DEPTH {
            return Err(Error::Malformed(format!(
                "directories nest more than {MAX_DEPTH} deep"
            )));
        }
        // `.` and `..` are ordinary dirents, so a walk that followed every entry would
        // go round for ever. Refusing a directory already on the way in catches the
        // two it is holding open as well as any other way back up.
        if !open.insert(nid) {
            return Err(Error::Malformed(format!(
                "directory {nid} is inside itself"
            )));
        }

        let mut directory = Directory::new(inode.meta.clone());
        for (name, child) in self.entries(nid, inode)? {
            if name == b"." || name == b".." {
                continue;
            }

            let child_inode = self.inode(child)?;
            let node = match child_inode.mode & S_IFMT {
                S_IFDIR => Node::Directory(self.directory(
                    child,
                    &child_inode,
                    tree,
                    ids,
                    open,
                    depth + 1,
                )?),

                S_IFREG => Node::File {
                    meta: child_inode.meta.clone(),
                    // One nid reached by two names is one inode, which is what a hard
                    // link is. The tree says so by giving both paths the same id.
                    id: *ids.entry(child).or_insert_with(|| tree.next_inode()),
                    data: Extent {
                        offset: u64::from(child_inode.u) * BLOCK_SIZE,
                        len: child_inode.size,
                    },
                },

                S_IFLNK => Node::Symlink {
                    meta: child_inode.meta.clone(),
                    target: self.contents(&child_inode)?,
                },

                kind @ (S_IFCHR | S_IFBLK) => {
                    let (major, minor) = decode_dev(child_inode.u);
                    Node::Device {
                        meta: child_inode.meta.clone(),
                        block: kind == S_IFBLK,
                        major,
                        minor,
                    }
                }

                S_IFIFO => Node::Fifo(child_inode.meta.clone()),

                other => {
                    return Err(Error::Unsupported(format!(
                        "{} is mode {other:o}, which a tree has no node for",
                        String::from_utf8_lossy(&name)
                    )));
                }
            };

            directory.entries.insert(name, node);
        }

        open.remove(&nid);
        Ok(directory)
    }

    /// A directory's entries, as `(name, nid)`.
    ///
    /// A block holds its dirents at the front and its names packed behind them, and
    /// where the names start is how many dirents there are: the first dirent's name
    /// offset is the end of the table. The last name in a block runs to the block's
    /// end, padded with the zeros a name cannot contain.
    fn entries(&mut self, nid: u64, inode: &Inode) -> Result<Vec<(Vec<u8>, u64)>> {
        let data = self.contents(inode)?;
        let malformed = |what: &str| Error::Malformed(format!("directory {nid}: {what}"));

        let mut entries = Vec::new();
        let mut at = 0;
        while at < data.len() {
            let block = &data[at..(at + BLOCK_SIZE as usize).min(data.len())];
            if block.len() < DIRENT_SIZE {
                return Err(malformed("a block too short to hold one entry"));
            }

            let table = u16::from_le_bytes(block[8..10].try_into().unwrap()) as usize;
            if table < DIRENT_SIZE || table > block.len() || !table.is_multiple_of(DIRENT_SIZE) {
                return Err(malformed("a name that does not start after the entries"));
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
                    return Err(malformed("a name outside the block holding it"));
                }

                // Only the last block is short; the others are padded out, and the
                // kernel takes a name to end at the first zero byte.
                let mut name = block[name_at..name_end].to_vec();
                while name.last() == Some(&0) {
                    name.pop();
                }
                if name.is_empty() {
                    return Err(malformed("an entry with no name"));
                }

                entries.push((name, u64::from_le_bytes(dirent[0..8].try_into().unwrap())));
            }

            at += BLOCK_SIZE as usize;
        }

        Ok(entries)
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::image::tree::Directory;

    fn meta() -> Meta {
        Meta {
            mode: 0o644,
            uid: 1000,
            gid: 100,
            mtime: 1_700_000_000,
            xattrs: Vec::new(),
        }
    }

    /// A tree, the bytes its files hold, and the archive those bytes come out of --
    /// which here is just the contents laid end to end.
    fn written(files: &[(&[u8], &[u8])]) -> (Tree, Vec<u8>) {
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

        (tree, tar)
    }

    fn image(tree: &Tree, tar: &[u8]) -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        super::super::write(tree, &mut Cursor::new(tar.to_vec()), &mut out).unwrap();
        out.into_inner()
    }

    /// The round trip that stitching rests on: what comes back names the same files,
    /// and each one's extent is where that file's bytes really are in the image.
    #[test]
    fn a_layer_says_where_its_own_bytes_are() {
        let (tree, tar) = written(&[
            (b"bin/sh", b"#!/bin/sh\n"),
            (b"etc/os-release", b"NAME=\"Alpine Linux\"\n"),
        ]);
        let bytes = image(&tree, &tar);

        let (read, layout) = read(&mut Cursor::new(bytes.clone())).unwrap();
        assert_eq!(layout.bytes, bytes.len() as u64);

        for (path, contents) in [
            (&b"bin/sh"[..], &b"#!/bin/sh\n"[..]),
            (b"etc/os-release", b"NAME=\"Alpine Linux\"\n"),
        ] {
            match read.get(path) {
                Some(Node::File { data, meta, .. }) => {
                    assert_eq!(data.len, contents.len() as u64);
                    let at = data.offset as usize;
                    assert_eq!(
                        &bytes[at..at + contents.len()],
                        contents,
                        "{} does not point at its own bytes",
                        String::from_utf8_lossy(path)
                    );
                    assert_eq!(meta.uid, 1000);
                    assert_eq!(meta.mtime, 1_700_000_000);
                }
                other => panic!("{} came back as {other:?}", String::from_utf8_lossy(path)),
            }
        }
    }

    #[test]
    fn what_is_not_a_file_comes_back_as_what_it_is() {
        let mut tree = Tree::new();
        let mut whiteout = meta();
        whiteout.mode = 0;
        tree.insert(
            b"etc/deleted",
            Node::Device {
                meta: whiteout,
                block: false,
                major: 0,
                minor: 0,
            },
        )
        .unwrap();
        tree.insert(
            b"etc/link",
            Node::Symlink {
                meta: meta(),
                target: b"/etc/os-release".to_vec(),
            },
        )
        .unwrap();
        tree.insert(b"dev/initctl", Node::Fifo(meta())).unwrap();
        tree.insert(
            b"dev/sda",
            Node::Device {
                meta: meta(),
                block: true,
                major: 8,
                minor: 300,
            },
        )
        .unwrap();

        let mut opaque = Directory::new(meta());
        opaque.meta.set_xattr(b"trusted.overlay.opaque", b"y");
        tree.insert(b"var/cache", Node::Directory(opaque)).unwrap();

        let (read, _) = read(&mut Cursor::new(image(&tree, &[]))).unwrap();

        assert!(matches!(
            read.get(b"etc/deleted"),
            Some(Node::Device {
                block: false,
                major: 0,
                minor: 0,
                ..
            })
        ));
        match read.get(b"etc/link") {
            Some(Node::Symlink { target, .. }) => assert_eq!(target, b"/etc/os-release"),
            other => panic!("the symlink came back as {other:?}"),
        }
        assert!(matches!(read.get(b"dev/initctl"), Some(Node::Fifo(_))));
        // The minor is wider than the byte the low half of a device number holds,
        // which is the case an encoding that only round trips small ones would lose.
        assert!(matches!(
            read.get(b"dev/sda"),
            Some(Node::Device {
                block: true,
                major: 8,
                minor: 300,
                ..
            })
        ));

        let cache = read.dir(b"var/cache").expect("var/cache");
        assert_eq!(
            cache
                .meta
                .xattrs
                .iter()
                .find(|xattr| xattr.name == b"trusted.overlay.opaque")
                .map(|xattr| xattr.value.as_slice()),
            Some(&b"y"[..]),
            "the xattr a merge reads deletions from did not survive"
        );
    }

    /// Two names for one inode have to come back as one inode, or a merge would count
    /// them as two files and the second would be written pointing at the first.
    #[test]
    fn a_hard_link_is_one_inode_on_the_way_back() {
        let mut tree = Tree::new();
        let id = tree.next_inode();
        for path in [&b"bin/busybox"[..], b"bin/sh"] {
            tree.insert(
                path,
                Node::File {
                    meta: meta(),
                    id,
                    data: Extent { offset: 0, len: 7 },
                },
            )
            .unwrap();
        }

        let (read, _) = read(&mut Cursor::new(image(&tree, b"busybox"))).unwrap();
        let ids: Vec<InodeId> = [&b"bin/busybox"[..], b"bin/sh"]
            .iter()
            .map(|path| match read.get(path) {
                Some(Node::File { id, .. }) => *id,
                other => panic!("{other:?}"),
            })
            .collect();

        assert_eq!(ids[0], ids[1], "one inode came back as two");
        assert_eq!(read.link_counts()[&ids[0]], 2);
    }

    /// A directory big enough to need several blocks of dirents, which is the one
    /// place the entry walk has more than one block to get right.
    #[test]
    fn a_directory_of_many_entries_comes_back_whole() {
        let mut tree = Tree::new();
        let names: Vec<String> = (0..400)
            .map(|n| format!("file-{n:04}-padded-name"))
            .collect();
        for name in &names {
            let id = tree.next_inode();
            tree.insert(
                format!("many/{name}").as_bytes(),
                Node::File {
                    meta: meta(),
                    id,
                    data: Extent { offset: 0, len: 0 },
                },
            )
            .unwrap();
        }

        let (read, _) = read(&mut Cursor::new(image(&tree, &[]))).unwrap();
        let many = read.dir(b"many").expect("many");
        assert_eq!(many.entries.len(), names.len());
        for name in &names {
            assert!(
                many.entries.contains_key(name.as_bytes()),
                "{name} did not come back"
            );
        }
    }

    #[test]
    fn what_is_not_an_image_is_refused() {
        let mut junk = vec![0u8; 8192];
        junk[..4].copy_from_slice(b"junk");
        assert!(matches!(
            read(&mut Cursor::new(junk)),
            Err(Error::Malformed(_))
        ));
        assert!(matches!(
            read(&mut Cursor::new(vec![0u8; 16])),
            Err(Error::Malformed(_))
        ));
    }
}
