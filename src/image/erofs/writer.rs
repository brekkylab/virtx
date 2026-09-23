//! Writing a [`Tree`] as an EROFS image.
//!
//! # Why this is three passes
//!
//! An inode's name is its address. `nid` counts 32-byte slots from the start of the
//! metadata area, so an inode is named by where it was placed, and where it lands
//! depends on the size of every inode before it -- which depends on their xattrs, and
//! on whether the tail of their data was small enough to tuck in behind them. A
//! directory entry names its child by that nid. Nothing can be written until
//! everything has been placed.
//!
//! So: [`Planner::plan`] walks the tree and decides where each inode and each data
//! block goes, [`Planner::pack`] serialises the directories now that the nids are
//! known, and [`write`] puts the bytes down.
//!
//! # The shape of the image
//!
//! ```text
//! block 0            boot sector, with the superblock at byte 1024
//! blocks 1..m        metadata: inodes, their xattrs, and any inlined tail
//! blocks m..n        data: whole blocks of file contents and directory entries
//! ```
//!
//! Metadata first means `meta_blkaddr` is 1 and the root inode is `nid` 0, which is
//! the only arrangement where the root is certain to be reachable: the superblock
//! holds the root's nid in 16 bits.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};

use super::format::*;
use crate::image::tree::{Directory, Extent, InodeId, Meta, Node, Tree, Xattr};
use crate::image::tree::{S_IFBLK, S_IFCHR, S_IFDIR, S_IFMT};

/// The block the metadata area starts at, and so `meta_blkaddr`.
const META_BLOCK: u64 = 1;

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    /// A directory entry whose name cannot fit in a block beside its dirent.
    NameTooLong(Vec<u8>),
    /// An inode whose inline xattrs leave it unable to fit in one block.
    XattrsTooLarge,
    /// The tar held fewer bytes than the tree says a file has, which means the tree
    /// and the archive it was read from have come apart.
    ShortRead,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(e) => write!(f, "{e}"),
            Error::NameTooLong(name) => {
                write!(
                    f,
                    "{} is too long to name a directory entry",
                    String::from_utf8_lossy(name)
                )
            }
            Error::XattrsTooLarge => write!(f, "an inode's xattrs do not fit beside it"),
            Error::ShortRead => write!(f, "the layer archive ended before a file it describes"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(source: std::io::Error) -> Error {
        Error::Io(source)
    }
}

type Result<T> = std::result::Result<T, Error>;

/// What an image came out as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    pub blocks: u32,
    pub bytes: u64,
    pub inodes: u64,
}

/// Write `tree` as an EROFS image on `out`.
///
/// `data` is the uncompressed tar the tree was read from; a file's bytes are fetched
/// from it rather than carried in the tree, so it has to be the same archive and
/// positioned wherever the caller likes. Nothing else is touched: no file is created,
/// renamed or removed, and `out` is written from its own byte zero.
pub fn write<D, W>(tree: &Tree, data: &mut D, out: &mut W) -> Result<Layout>
where
    D: Read + Seek,
    W: Write + Seek,
{
    let planner = Planner::plan(tree)?;

    let meta_end = align_block(planner.meta);
    let data_start = (meta_end / BLOCK_SIZE) as u32;
    let blocks = data_start + planner.block;

    // Block 0 is the boot sector. The superblock goes in it, but only once the
    // totals below are known, so it is left as zeros and patched at the end.
    out.rewind()?;
    pad(out, BLOCK_SIZE)?;

    // The metadata area, in placement order, which is the order the plans are in.
    let mut cursor = META_BLOCK * BLOCK_SIZE;
    for plan in &planner.plans {
        pad(out, plan.meta_offset - cursor)?;
        out.write_all(&inode(plan, data_start))?;
        out.write_all(&plan.xattrs)?;
        if plan.tail > 0 {
            body(
                plan,
                &planner.dirs,
                data,
                out,
                plan.size - plan.tail,
                plan.tail,
            )?;
        }
        cursor = plan.meta_offset + INODE_SIZE + plan.xattrs.len() as u64 + plan.tail;
    }
    pad(out, meta_end - cursor)?;

    // The data area. Blocks were handed out in plan order, so writing them in plan
    // order writes them in address order.
    for plan in &planner.plans {
        if plan.blocks == 0 {
            continue;
        }
        let bytes = plan.size - plan.tail;
        body(plan, &planner.dirs, data, out, 0, bytes)?;
        pad(out, u64::from(plan.blocks) * BLOCK_SIZE - bytes)?;
    }

    out.seek(SeekFrom::Start(SUPER_OFFSET))?;
    out.write_all(&superblock(&planner, blocks))?;
    out.flush()?;

    Ok(Layout {
        blocks,
        bytes: u64::from(blocks) * BLOCK_SIZE,
        inodes: planner.plans.len() as u64,
    })
}

//--------------------------------------------------------------------------------------------------
// Planning
//--------------------------------------------------------------------------------------------------

/// Where one inode goes and what it holds.
struct Plan {
    /// Byte offset of the inode within the image.
    meta_offset: u64,
    nid: u64,
    mode: u16,
    uid: u32,
    gid: u32,
    mtime: i64,
    nlink: u32,
    ino: u32,
    /// `i_u` for a device, which is the only inode whose `i_u` is not an address.
    rdev: u32,
    size: u64,
    layout: u16,
    /// First data block, counted from the start of the data area.
    first_block: u32,
    blocks: u32,
    /// Bytes of the body carried in the metadata area, right behind the inode.
    tail: u64,
    xattrs: Vec<u8>,
    xattr_icount: u16,
    body: Body,
}

/// Where an inode's bytes come from.
enum Body {
    /// Nothing to write: a device, a fifo, a socket, an empty file.
    Empty,
    /// A range of the uncompressed tar.
    Tar(Extent),
    /// Small and already in hand: a symlink target.
    Bytes(Vec<u8>),
    /// Directory entries, which are serialised once every nid is known.
    Dir(usize),
}

/// A directory, held apart from its [`Plan`] because its contents cannot be built
/// until the whole tree is placed.
struct DirPlan {
    /// `(name, plan)` in the order EROFS wants dirents in, `.` and `..` included.
    entries: Vec<(Vec<u8>, usize)>,
    /// How many of `entries` go in each block.
    groups: Vec<usize>,
    bytes: Vec<u8>,
}

struct Planner {
    plans: Vec<Plan>,
    dirs: Vec<DirPlan>,
    /// How many paths point at each inode, so a hard link gets the right `i_nlink`.
    links: HashMap<InodeId, u32>,
    /// Inodes already placed, so the second path to one reuses it rather than
    /// copying it.
    placed: HashMap<InodeId, usize>,
    /// Running offset in the metadata area.
    meta: u64,
    /// Running block in the data area.
    block: u32,
}

impl Planner {
    fn plan(tree: &Tree) -> Result<Planner> {
        let mut planner = Planner {
            plans: Vec::new(),
            dirs: Vec::new(),
            links: tree.link_counts(),
            placed: HashMap::new(),
            meta: META_BLOCK * BLOCK_SIZE,
            block: 0,
        };

        // The root's parent is the root, which is plan 0 -- the first thing placed.
        planner.plan_dir(&tree.root, 0)?;
        planner.pack();

        Ok(planner)
    }

    /// Decide where one inode goes, and how much of its body travels with it.
    fn place(
        &mut self,
        mode: u16,
        meta: &Meta,
        size: u64,
        rdev: u32,
        nlink: u32,
        body: Body,
    ) -> Result<usize> {
        let (xattrs, xattr_icount) = encode_xattrs(&meta.xattrs);

        // The inode and its xattrs are read as one, so they may not straddle a block.
        let header = INODE_SIZE + xattrs.len() as u64;
        if header > BLOCK_SIZE {
            return Err(Error::XattrsTooLarge);
        }
        if self.meta % BLOCK_SIZE + header > BLOCK_SIZE {
            self.meta = align_block(self.meta);
        }

        // A body that ends part-way through a block can put that remainder behind the
        // inode instead of padding a block out with it -- but only if it fits in what
        // is left of this one.
        let meta_offset = self.meta;
        let behind = meta_offset + header;

        // A layer image is an extent under a merged metadata image, not a filesystem
        // anything mounts on its own, and an extent can only be addressed by whole
        // blocks. `FLAT_INLINE` puts a file's last partial block behind its inode --
        // in the metadata the merge replaces -- so a file laid out that way has bytes
        // the merged image cannot point at. Every file therefore keeps all of itself
        // in the data area, at the cost of the padding a tail would have saved.
        let inline = false;
        let remainder = size % BLOCK_SIZE;
        let tail = if inline { remainder } else { 0 };

        let in_blocks = size - tail;
        let blocks = in_blocks.div_ceil(BLOCK_SIZE) as u32;
        let first_block = self.block;
        self.block += blocks;

        self.meta = (behind + tail).div_ceil(SLOT_SIZE) * SLOT_SIZE;

        let index = self.plans.len();
        self.plans.push(Plan {
            meta_offset,
            nid: (meta_offset - META_BLOCK * BLOCK_SIZE) / SLOT_SIZE,
            mode,
            uid: meta.uid,
            gid: meta.gid,
            mtime: meta.mtime,
            nlink,
            ino: index as u32 + 1,
            rdev,
            size,
            layout: if inline {
                LAYOUT_FLAT_INLINE
            } else {
                LAYOUT_FLAT_PLAIN
            },
            first_block,
            blocks,
            tail,
            xattrs,
            xattr_icount,
            body,
        });

        Ok(index)
    }

    fn plan_dir(&mut self, dir: &Directory, parent: usize) -> Result<usize> {
        // `.` and `..` are ordinary entries on disk and sort among the rest. The
        // kernel bisects a directory by name, so the order is not cosmetic.
        let mut names: Vec<&[u8]> = Vec::with_capacity(dir.entries.len() + 2);
        names.push(b".");
        names.push(b"..");
        names.extend(dir.entries.keys().map(Vec::as_slice));
        names.sort_unstable();

        let (groups, size) = group_dirents(&names)?;

        let index = self.dirs.len();
        self.dirs.push(DirPlan {
            entries: Vec::new(),
            groups,
            bytes: Vec::new(),
        });

        // A directory is linked to by its parent, by its own `.`, and by the `..` of
        // each directory under it.
        let children = dir.entries.values().filter(|node| node.is_dir()).count();
        let plan = self.place(
            S_IFDIR | (dir.meta.mode & 0o7777),
            &dir.meta,
            size,
            0,
            2 + children as u32,
            Body::Dir(index),
        )?;

        let mut entries = Vec::with_capacity(names.len());
        for name in names {
            let target = match name {
                b"." => plan,
                b".." => parent,
                name => self.plan_node(&dir.entries[name], plan)?,
            };
            entries.push((name.to_vec(), target));
        }
        self.dirs[index].entries = entries;

        Ok(plan)
    }

    fn plan_node(&mut self, node: &Node, parent: usize) -> Result<usize> {
        let mode = node.mode();

        match node {
            Node::Directory(dir) => self.plan_dir(dir, parent),

            Node::File { meta, id, data } => {
                // The second path to a hard-linked inode is a dirent, not an inode.
                if let Some(placed) = self.placed.get(id) {
                    return Ok(*placed);
                }
                let nlink = self.links.get(id).copied().unwrap_or(1);
                let plan = self.place(mode, meta, data.len, 0, nlink, Body::Tar(*data))?;
                self.placed.insert(*id, plan);
                Ok(plan)
            }

            Node::Symlink { meta, target } => self.place(
                mode,
                meta,
                target.len() as u64,
                0,
                1,
                Body::Bytes(target.clone()),
            ),

            Node::Device {
                meta, major, minor, ..
            } => self.place(mode, meta, 0, encode_dev(*major, *minor), 1, Body::Empty),

            Node::Fifo(meta) => self.place(mode, meta, 0, 0, 1, Body::Empty),
        }
    }

    /// Serialise every directory, which is only possible now that each entry's nid
    /// is settled.
    fn pack(&mut self) {
        let packed: Vec<Vec<u8>> = self
            .dirs
            .iter()
            .map(|dir| pack_dirents(dir, &self.plans))
            .collect();

        for (dir, bytes) in self.dirs.iter_mut().zip(packed) {
            dir.bytes = bytes;
        }
    }
}

//--------------------------------------------------------------------------------------------------
// Encoding
//--------------------------------------------------------------------------------------------------

/// Split a directory's entries into blocks, and say how long the result is.
///
/// A block holds its dirents at the front and the names packed behind them, so an
/// entry costs twelve bytes plus its name and blocks fill greedily. Every block but
/// the last is whole; the last is only as long as it needs to be, and the kernel
/// knows where it ends because that is what `i_size` says.
fn group_dirents(names: &[&[u8]]) -> Result<(Vec<usize>, u64)> {
    let block = BLOCK_SIZE as usize;
    let mut groups = Vec::new();
    let mut size = 0;
    let mut index = 0;

    while index < names.len() {
        let mut count = 0;
        let mut used = 0;

        while index + count < names.len() {
            let name = names[index + count];
            if DIRENT_SIZE + name.len() > block {
                return Err(Error::NameTooLong(name.to_vec()));
            }
            if used + DIRENT_SIZE + name.len() > block {
                break;
            }
            used += DIRENT_SIZE + name.len();
            count += 1;
        }

        groups.push(count);
        index += count;
        size += if index < names.len() {
            BLOCK_SIZE
        } else {
            used as u64
        };
    }

    Ok((groups, size))
}

fn pack_dirents(dir: &DirPlan, plans: &[Plan]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut index = 0;

    for (block, count) in dir.groups.iter().enumerate() {
        let entries = &dir.entries[index..index + count];
        let names: usize = entries.iter().map(|(name, _)| name.len()).sum();
        let used = count * DIRENT_SIZE + names;

        // Only the last block is short. The others are padded, and the kernel takes
        // a name to end at the first zero byte, so the padding reads as nothing.
        let last = block + 1 == dir.groups.len();
        let mut buffer = vec![0u8; if last { used } else { BLOCK_SIZE as usize }];

        let mut name_at = count * DIRENT_SIZE;
        for (slot, (name, target)) in entries.iter().enumerate() {
            let plan = &plans[*target];
            let at = slot * DIRENT_SIZE;

            buffer[at..at + 8].copy_from_slice(&plan.nid.to_le_bytes());
            buffer[at + 8..at + 10].copy_from_slice(&(name_at as u16).to_le_bytes());
            buffer[at + 10] = file_type(plan.mode);
            buffer[name_at..name_at + name.len()].copy_from_slice(name);
            name_at += name.len();
        }

        out.extend_from_slice(&buffer);
        index += count;
    }

    out
}

/// An inode's inline xattrs, and the `i_xattr_icount` that describes their length.
///
/// A name is stored as a prefix index and the rest, so a name with a prefix EROFS
/// has no index for cannot be written at all. Those are dropped rather than refused:
/// a layer that carries one is still a layer worth booting, and the alternative is
/// a pull that fails on an image `podman` would have run.
fn encode_xattrs(xattrs: &[Xattr]) -> (Vec<u8>, u16) {
    let storable: Vec<(u8, &[u8], &[u8])> = xattrs
        .iter()
        .filter_map(|xattr| {
            let (index, suffix) = xattr_prefix(&xattr.name)?;
            (suffix.len() <= u8::MAX as usize && xattr.value.len() <= u16::MAX as usize)
                .then_some((index, suffix, xattr.value.as_slice()))
        })
        .collect();

    if storable.is_empty() {
        return (Vec::new(), 0);
    }

    let mut body = vec![0u8; XATTR_HEADER_SIZE as usize];
    for (index, suffix, value) in storable {
        let start = body.len();
        body.push(suffix.len() as u8);
        body.push(index);
        body.extend_from_slice(&(value.len() as u16).to_le_bytes());
        body.extend_from_slice(suffix);
        body.extend_from_slice(value);
        body.resize(start + xattr_align((body.len() - start) as u64) as usize, 0);
    }

    let icount = ((body.len() as u64 - XATTR_HEADER_SIZE) / 4 + 1) as u16;
    (body, icount)
}

fn inode(plan: &Plan, data_start: u32) -> [u8; INODE_SIZE as usize] {
    let mut inode = [0u8; INODE_SIZE as usize];

    // `i_u` is a device number for a device and a data address for everything else.
    // A whiteout is a character device numbered 0:0, so this has to be by type and
    // not by whether the number looks empty.
    let u = match plan.mode & S_IFMT {
        S_IFCHR | S_IFBLK => plan.rdev,
        _ => data_start + plan.first_block,
    };

    inode[0..2].copy_from_slice(&(INODE_VERSION_EXTENDED | (plan.layout << 1)).to_le_bytes());
    inode[2..4].copy_from_slice(&plan.xattr_icount.to_le_bytes());
    inode[4..6].copy_from_slice(&plan.mode.to_le_bytes());
    inode[8..16].copy_from_slice(&plan.size.to_le_bytes());
    inode[16..20].copy_from_slice(&u.to_le_bytes());
    inode[20..24].copy_from_slice(&plan.ino.to_le_bytes());
    inode[24..28].copy_from_slice(&plan.uid.to_le_bytes());
    inode[28..32].copy_from_slice(&plan.gid.to_le_bytes());
    inode[32..40].copy_from_slice(&plan.mtime.to_le_bytes());
    inode[44..48].copy_from_slice(&plan.nlink.to_le_bytes());

    inode
}

fn superblock(planner: &Planner, blocks: u32) -> [u8; SUPER_SIZE] {
    let mut sb = [0u8; SUPER_SIZE];

    sb[0x00..0x04].copy_from_slice(&SUPER_MAGIC.to_le_bytes());
    // 0x04..0x08 is the checksum, which is over a superblock that says zero here.
    sb[0x08..0x0C].copy_from_slice(&FEATURE_COMPAT_SB_CHKSUM.to_le_bytes());
    sb[0x0C] = BLOCK_BITS;
    // The root is the first inode placed, so its nid is 0 and the 16 bits the
    // superblock gives it are never in question.
    sb[0x0E..0x10].copy_from_slice(&(planner.plans[0].nid as u16).to_le_bytes());
    sb[0x10..0x18].copy_from_slice(&(planner.plans.len() as u64).to_le_bytes());
    sb[0x24..0x28].copy_from_slice(&blocks.to_le_bytes());
    sb[0x28..0x2C].copy_from_slice(&(META_BLOCK as u32).to_le_bytes());

    // The checksum covers the whole of block 0 past the boot sector: the superblock
    // and the zeros after it.
    let mut block = [0u8; (BLOCK_SIZE - SUPER_OFFSET) as usize];
    block[..SUPER_SIZE].copy_from_slice(&sb);
    sb[0x04..0x08].copy_from_slice(&crc32c(0xFFFF_FFFF, &block).to_le_bytes());

    sb
}

//--------------------------------------------------------------------------------------------------
// Writing
//--------------------------------------------------------------------------------------------------

/// Write `len` bytes of an inode's body, starting `from` bytes into it.
fn body<D, W>(
    plan: &Plan,
    dirs: &[DirPlan],
    data: &mut D,
    out: &mut W,
    from: u64,
    len: u64,
) -> Result<()>
where
    D: Read + Seek,
    W: Write,
{
    if len == 0 {
        return Ok(());
    }

    let held = |bytes: &[u8]| -> Result<Vec<u8>> {
        bytes
            .get(from as usize..(from + len) as usize)
            .map(<[u8]>::to_vec)
            .ok_or(Error::ShortRead)
    };

    match &plan.body {
        Body::Empty => return Err(Error::ShortRead),
        Body::Bytes(bytes) => out.write_all(&held(bytes)?)?,
        Body::Dir(index) => out.write_all(&held(&dirs[*index].bytes)?)?,
        Body::Tar(extent) => {
            data.seek(SeekFrom::Start(extent.offset + from))?;

            let mut buffer = [0u8; 64 * 1024];
            let mut left = len;
            while left > 0 {
                let want = left.min(buffer.len() as u64) as usize;
                data.read_exact(&mut buffer[..want])
                    .map_err(|_| Error::ShortRead)?;
                out.write_all(&buffer[..want])?;
                left -= want as u64;
            }
        }
    }

    Ok(())
}

fn pad(out: &mut impl Write, len: u64) -> Result<()> {
    const ZEROS: [u8; BLOCK_SIZE as usize] = [0; BLOCK_SIZE as usize];

    let mut left = len;
    while left > 0 {
        let want = left.min(ZEROS.len() as u64) as usize;
        out.write_all(&ZEROS[..want])?;
        left -= want as u64;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::tree::{Meta, S_IFLNK, S_IFREG};
    use std::io::Cursor;

    /// Just enough of a reader to say whether what was written is what the kernel
    /// will find. Written from the format rather than from [`write`], so that a
    /// mistake shared by both would have to be made twice.
    struct Image {
        bytes: Vec<u8>,
        meta: u64,
        root: u64,
    }

    struct Inode {
        layout: u16,
        mode: u16,
        size: u64,
        u: u32,
        nlink: u32,
        uid: u32,
        /// Offset just past the inode and its xattrs, where an inlined tail starts.
        behind: u64,
        xattrs: Vec<(u8, Vec<u8>, Vec<u8>)>,
    }

    impl Image {
        fn read(bytes: Vec<u8>) -> Image {
            let sb = &bytes[SUPER_OFFSET as usize..][..SUPER_SIZE];
            assert_eq!(
                u32::from_le_bytes(sb[0..4].try_into().unwrap()),
                SUPER_MAGIC
            );
            assert_eq!(sb[0x0C], BLOCK_BITS);

            let blocks = u32::from_le_bytes(sb[0x24..0x28].try_into().unwrap());
            assert_eq!(bytes.len() as u64, u64::from(blocks) * BLOCK_SIZE);

            let checksum = u32::from_le_bytes(sb[0x04..0x08].try_into().unwrap());
            let mut zeroed = bytes[SUPER_OFFSET as usize..BLOCK_SIZE as usize].to_vec();
            zeroed[4..8].fill(0);
            assert_eq!(
                crc32c(0xFFFF_FFFF, &zeroed),
                checksum,
                "superblock checksum"
            );

            let meta = u64::from(u32::from_le_bytes(sb[0x28..0x2C].try_into().unwrap()));
            let root = u64::from(u16::from_le_bytes(sb[0x0E..0x10].try_into().unwrap()));

            Image {
                bytes,
                meta: meta * BLOCK_SIZE,
                root,
            }
        }

        fn inode(&self, nid: u64) -> Inode {
            let at = (self.meta + nid * SLOT_SIZE) as usize;
            let raw = &self.bytes[at..at + INODE_SIZE as usize];

            let format = u16::from_le_bytes(raw[0..2].try_into().unwrap());
            assert_eq!(format & 1, INODE_VERSION_EXTENDED, "extended inode");

            let icount = u16::from_le_bytes(raw[2..4].try_into().unwrap());
            let xattr_size = if icount == 0 {
                0
            } else {
                XATTR_HEADER_SIZE + 4 * u64::from(icount - 1)
            };

            let mut xattrs = Vec::new();
            let mut cursor = at as u64 + INODE_SIZE + XATTR_HEADER_SIZE;
            let end = at as u64 + INODE_SIZE + xattr_size;
            while cursor < end {
                let entry = &self.bytes[cursor as usize..];
                let name_len = entry[0] as usize;
                let index = entry[1];
                let value_len = u16::from_le_bytes(entry[2..4].try_into().unwrap()) as usize;
                xattrs.push((
                    index,
                    entry[4..4 + name_len].to_vec(),
                    entry[4 + name_len..4 + name_len + value_len].to_vec(),
                ));
                cursor += xattr_align((4 + name_len + value_len) as u64);
            }

            Inode {
                layout: (format >> 1) & 0b111,
                mode: u16::from_le_bytes(raw[4..6].try_into().unwrap()),
                size: u64::from_le_bytes(raw[8..16].try_into().unwrap()),
                u: u32::from_le_bytes(raw[16..20].try_into().unwrap()),
                uid: u32::from_le_bytes(raw[24..28].try_into().unwrap()),
                nlink: u32::from_le_bytes(raw[44..48].try_into().unwrap()),
                behind: at as u64 + INODE_SIZE + xattr_size,
                xattrs,
            }
        }

        /// An inode's contents, gathered the way the kernel gathers them: whole
        /// blocks from the data area, and the remainder from behind the inode.
        fn contents(&self, inode: &Inode) -> Vec<u8> {
            let tail = if inode.layout == LAYOUT_FLAT_INLINE {
                inode.size % BLOCK_SIZE
            } else {
                0
            };

            let from_blocks = inode.size - tail;
            let mut out = Vec::with_capacity(inode.size as usize);
            let at = u64::from(inode.u) * BLOCK_SIZE;
            out.extend_from_slice(&self.bytes[at as usize..(at + from_blocks) as usize]);
            out.extend_from_slice(
                &self.bytes[inode.behind as usize..(inode.behind + tail) as usize],
            );

            out
        }

        /// A directory's entries, as `(name, nid, file type)`.
        fn entries(&self, nid: u64) -> Vec<(Vec<u8>, u64, u8)> {
            let inode = self.inode(nid);
            assert_eq!(inode.mode & S_IFMT, S_IFDIR);
            let data = self.contents(&inode);

            let mut entries = Vec::new();
            let mut at = 0;
            while at < data.len() {
                let block = &data[at..(at + BLOCK_SIZE as usize).min(data.len())];
                let first = u16::from_le_bytes(block[8..10].try_into().unwrap()) as usize;
                let count = first / DIRENT_SIZE;

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

                    let mut name = block[name_at..name_end].to_vec();
                    // The last name in a padded block runs to the block's end, and
                    // the padding is zeros.
                    while name.last() == Some(&0) {
                        name.pop();
                    }

                    entries.push((
                        name,
                        u64::from_le_bytes(dirent[0..8].try_into().unwrap()),
                        dirent[10],
                    ));
                }

                at += BLOCK_SIZE as usize;
            }

            entries
        }

        fn lookup(&self, path: &[&[u8]]) -> u64 {
            let mut nid = self.root;
            for name in path {
                nid = self
                    .entries(nid)
                    .into_iter()
                    .find(|(entry, _, _)| entry == name)
                    .unwrap_or_else(|| panic!("no {} here", String::from_utf8_lossy(name)))
                    .1;
            }
            nid
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

    /// Build an image from a tree whose file contents are `data`, laid end to end.
    fn build(tree: &Tree, data: &[u8]) -> Image {
        let mut source = Cursor::new(data.to_vec());
        let mut out = Cursor::new(Vec::new());
        write(tree, &mut source, &mut out).expect("write");
        Image::read(out.into_inner())
    }

    fn file(tree: &mut Tree, at: u64, len: u64) -> Node {
        Node::File {
            meta: meta(),
            id: tree.next_inode(),
            data: Extent { offset: at, len },
        }
    }

    #[test]
    fn an_empty_tree_is_a_mountable_image() {
        let image = build(&Tree::new(), b"");

        let entries = image.entries(image.root);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].0, b".");
        assert_eq!(entries[1].0, b"..");
        // Both point back at the root, which is its own parent.
        assert_eq!(entries[0].1, image.root);
        assert_eq!(entries[1].1, image.root);
        assert_eq!(image.inode(image.root).nlink, 2);
    }

    #[test]
    fn a_small_file_still_gets_a_block_of_its_own() {
        let data = b"hello from a layer";
        let mut tree = Tree::new();
        let node = file(&mut tree, 0, data.len() as u64);
        tree.insert(b"greeting", node).unwrap();

        let image = build(&tree, data);
        let inode = image.inode(image.lookup(&[b"greeting"]));

        assert_eq!(inode.layout, LAYOUT_FLAT_PLAIN);
        assert_eq!(inode.size, data.len() as u64);
        assert_eq!(image.contents(&inode), data);
    }

    #[test]
    fn a_file_over_a_block_keeps_whole_blocks_in_the_data_area() {
        let data: Vec<u8> = (0..BLOCK_SIZE * 2 + 17).map(|i| (i % 251) as u8).collect();
        let mut tree = Tree::new();
        let node = file(&mut tree, 0, data.len() as u64);
        tree.insert(b"big", node).unwrap();

        let image = build(&tree, &data);
        let inode = image.inode(image.lookup(&[b"big"]));

        assert_eq!(inode.layout, LAYOUT_FLAT_PLAIN);
        assert_eq!(inode.size, data.len() as u64);
        assert_eq!(image.contents(&inode), data);
    }

    #[test]
    fn a_file_that_is_whole_blocks_has_no_tail() {
        let data: Vec<u8> = (0..BLOCK_SIZE).map(|i| (i % 251) as u8).collect();
        let mut tree = Tree::new();
        let node = file(&mut tree, 0, data.len() as u64);
        tree.insert(b"exact", node).unwrap();

        let image = build(&tree, &data);
        let inode = image.inode(image.lookup(&[b"exact"]));

        assert_eq!(inode.layout, LAYOUT_FLAT_PLAIN);
        assert_eq!(image.contents(&inode), data);
    }

    #[test]
    fn directories_nest_and_know_their_parents() {
        let mut tree = Tree::new();
        let node = file(&mut tree, 0, 3);
        tree.insert(b"usr/lib/libc.so", node).unwrap();

        let image = build(&tree, b"abc");
        let usr = image.lookup(&[b"usr"]);
        let lib = image.lookup(&[b"usr", b"lib"]);

        assert_eq!(image.inode(usr).mode & S_IFMT, S_IFDIR);
        // `..` of usr/lib is usr, and `.` is itself.
        let entries = image.entries(lib);
        assert_eq!(entries.iter().find(|e| e.0 == b".").unwrap().1, lib);
        assert_eq!(entries.iter().find(|e| e.0 == b"..").unwrap().1, usr);
        // usr holds one directory, so its link count is itself, its parent and lib.
        assert_eq!(image.inode(usr).nlink, 3);
    }

    #[test]
    fn a_hard_link_is_one_inode_under_two_names() {
        let mut tree = Tree::new();
        let node = file(&mut tree, 0, 7);
        let id = match &node {
            Node::File { id, .. } => *id,
            _ => unreachable!(),
        };
        tree.insert(b"bin/busybox", node).unwrap();
        tree.insert(
            b"bin/sh",
            Node::File {
                meta: meta(),
                id,
                data: Extent { offset: 0, len: 7 },
            },
        )
        .unwrap();

        let image = build(&tree, b"busybox");
        let busybox = image.lookup(&[b"bin", b"busybox"]);
        let sh = image.lookup(&[b"bin", b"sh"]);

        assert_eq!(busybox, sh, "both names reach the same inode");
        assert_eq!(image.inode(busybox).nlink, 2);
    }

    #[test]
    fn a_whiteout_is_a_character_device_numbered_zero() {
        let mut tree = Tree::new();
        tree.insert(
            b"etc/hosts",
            Node::Device {
                meta: Meta { mode: 0, ..meta() },
                block: false,
                major: 0,
                minor: 0,
            },
        )
        .unwrap();

        let image = build(&tree, b"");
        let inode = image.inode(image.lookup(&[b"etc", b"hosts"]));

        assert_eq!(inode.mode & S_IFMT, S_IFCHR);
        assert_eq!(inode.u, 0, "device 0:0");
        assert_eq!(
            image
                .entries(image.lookup(&[b"etc"]))
                .iter()
                .find(|e| e.0 == b"hosts")
                .unwrap()
                .2,
            FT_CHRDEV
        );
    }

    #[test]
    fn an_opaque_directory_carries_the_xattr_overlayfs_looks_for() {
        let mut tree = Tree::new();
        tree.insert(b"var/cache/x", file(&mut Tree::new(), 0, 0))
            .unwrap();
        tree.dir_mut(b"var/cache")
            .unwrap()
            .meta
            .set_xattr(b"trusted.overlay.opaque", b"y");

        let image = build(&tree, b"");
        let inode = image.inode(image.lookup(&[b"var", b"cache"]));

        assert_eq!(
            inode.xattrs,
            vec![(
                XATTR_INDEX_TRUSTED,
                b"overlay.opaque".to_vec(),
                b"y".to_vec()
            )]
        );
    }

    #[test]
    fn a_symlink_holds_its_target() {
        let mut tree = Tree::new();
        tree.insert(
            b"bin/sh",
            Node::Symlink {
                meta: meta(),
                target: b"/bin/busybox".to_vec(),
            },
        )
        .unwrap();

        let image = build(&tree, b"");
        let inode = image.inode(image.lookup(&[b"bin", b"sh"]));

        assert_eq!(inode.mode & S_IFMT, S_IFLNK);
        assert_eq!(image.contents(&inode), b"/bin/busybox");
    }

    #[test]
    fn a_directory_wider_than_a_block_spans_blocks_in_name_order() {
        let mut tree = Tree::new();
        // Names long enough that a few hundred will not fit one block.
        for index in 0..400 {
            let name = format!("entry-{index:04}-{}", "x".repeat(40));
            let node = file(&mut tree, 0, 0);
            tree.insert(name.as_bytes(), node).unwrap();
        }

        let image = build(&tree, b"");
        let entries = image.entries(image.root);

        assert_eq!(entries.len(), 402, ". and .. and the 400");
        let names: Vec<&[u8]> = entries.iter().map(|(name, _, _)| name.as_slice()).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted, "dirents are sorted by name");

        // Every entry still resolves, which is what spanning blocks can break.
        for (name, nid, _) in &entries {
            if name == b"." || name == b".." {
                continue;
            }
            assert_eq!(image.inode(*nid).mode & S_IFMT, S_IFREG);
        }
    }

    #[test]
    fn xattrs_push_an_inode_along_without_breaking_the_one_after_it() {
        let mut tree = Tree::new();
        for index in 0..64 {
            let mut meta = meta();
            meta.set_xattr(
                format!("user.attribute-{index}").as_bytes(),
                &vec![b'v'; index * 7 % 100],
            );
            let id = tree.next_inode();
            tree.insert(
                format!("file-{index:02}").as_bytes(),
                Node::File {
                    meta,
                    id,
                    data: Extent { offset: 0, len: 5 },
                },
            )
            .unwrap();
        }

        let image = build(&tree, b"hello");
        for index in 0..64 {
            let nid = image.lookup(&[format!("file-{index:02}").as_bytes()]);
            let inode = image.inode(nid);
            assert_eq!(inode.mode & S_IFMT, S_IFREG, "file-{index:02}");
            assert_eq!(image.contents(&inode), b"hello", "file-{index:02}");
            assert_eq!(inode.xattrs.len(), 1, "file-{index:02}");
        }
    }

    #[test]
    fn uid_and_gid_survive_being_larger_than_a_compact_inode_would_hold() {
        let mut tree = Tree::new();
        let mut meta = meta();
        meta.uid = 100_000;
        let id = tree.next_inode();
        tree.insert(
            b"owned",
            Node::File {
                meta,
                id,
                data: Extent { offset: 0, len: 0 },
            },
        )
        .unwrap();

        let image = build(&tree, b"");
        assert_eq!(image.inode(image.lookup(&[b"owned"])).uid, 100_000);
    }
}
