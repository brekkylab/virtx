//! Writing an [`FsTree`](crate::FsTree) as an EROFS image.
//!
//! # Why this is three passes
//!
//! An inode's name is its address. `nid` counts 32-byte slots from the start of the
//! metadata area, so an inode is named by where it was placed, and where it lands depends
//! on the size of every inode before it -- which depends on their xattrs. A directory
//! entry names its child by that nid. Nothing can be written until everything has been
//! placed.
//!
//! So: [`Planner::plan`] walks the tree and decides where each inode and each data block
//! goes, [`Planner::pack`] serialises the directories now that the nids are known, and
//! [`emit`] puts the bytes down.
//!
//! # The shape of the image
//!
//! ```text
//! block 0            boot sector, with the superblock at byte 1024
//! blocks 1..m        metadata: inodes and their xattrs
//! blocks m..n        data: whole blocks of file contents and directory entries
//! ```
//!
//! Metadata first means `meta_blkaddr` is 1 and the root inode is `nid` 0, which is the
//! only arrangement where the root is certain to be reachable: the superblock holds the
//! root's nid in 16 bits.
//!
//! No file's tail is inlined behind its inode. EROFS allows it and it saves the padding,
//! but those bytes would then sit in the metadata a merge of several layers replaces,
//! and a merged image could not point at them. Every file keeps all of itself in the
//! data area, in whole blocks.
//!
//! # Behind the image
//!
//! A file whose contents are a [`Span`] has no blocks here at all. Its bytes are in
//! another image -- a layer the merged image is put in front of, on one disk -- and its
//! inode points past the image's own last block to them. Each image pointed into is placed
//! behind this one once, in the order the walk first reaches it, and that order is what the
//! writer answers with: it is the disk's, and nothing else knows it. The superblock counts
//! those images' blocks too, so that what it says the filesystem spans is the disk and not
//! the front of it.

use std::collections::{BTreeMap, HashMap};
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::Arc;

use super::format::*;
use crate::fstree::{Entry, Error, FsTree, Inode, Meta, Result, Span, Timestamp};

/// The block the metadata area starts at, and so `meta_blkaddr`.
const META_BLOCK: u64 = 1;

/// A file's contents, as the planner places them: here, or behind the image.
pub(super) enum Contents<'a> {
    Bytes(&'a [u8]),
    Span(&'a Span),
}

/// What a tree's files hold, said as [`Contents`].
pub(super) trait FileData {
    fn contents(&self) -> Contents<'_>;
}

impl FileData for Vec<u8> {
    fn contents(&self) -> Contents<'_> {
        Contents::Bytes(self)
    }
}

impl FileData for Span {
    fn contents(&self) -> Contents<'_> {
        Contents::Span(self)
    }
}

/// Write `tree` onto `out`, and answer with the images behind it.
pub(super) fn write<D: FileData>(
    tree: &FsTree<D>,
    out: impl Write + Seek,
) -> Result<Vec<Arc<Path>>> {
    let walked = tree.walk();
    let planner = Planner::plan(&walked)?;

    // Every address is a 32-bit block number, and the furthest one is the image's own
    // length and then every image behind it.
    let total = u64::from(planner.blocks()) + planner.behind_blocks;
    let total = u32::try_from(total).map_err(|_| {
        Error::Unwritable(format!(
            "EROFS image: {total} blocks, which is more than an EROFS image can address"
        ))
    })?;

    let mut buffered = BufWriter::new(out);
    emit(&planner, total, &mut buffered).map_err(|e| Error::io("writing an EROFS image", e))?;
    buffered
        .into_inner()
        .map_err(|e| Error::io("writing an EROFS image", e.into_error()))?;
    Ok(planner.behind)
}

/// Put a planned image down, `total` blocks being what its superblock says it spans.
fn emit(planner: &Planner, total: u32, out: &mut (impl Write + Seek)) -> std::io::Result<()> {
    let meta_end = align_block(planner.meta);
    let data_start = (meta_end / BLOCK_SIZE) as u32;
    // Where what lies behind the image starts: past its data area too.
    let behind_start = planner.blocks();

    // Block 0 is the boot sector. The superblock goes in it, but only once the totals
    // are known, so it is left as zeros and patched at the end.
    out.rewind()?;
    pad(out, BLOCK_SIZE)?;

    // The metadata area, in placement order, which is the order the plans are in.
    let mut cursor = META_BLOCK * BLOCK_SIZE;
    for plan in &planner.plans {
        pad(out, plan.meta_offset - cursor)?;
        out.write_all(&inode(plan, data_start, behind_start))?;
        out.write_all(&plan.xattrs)?;
        cursor = plan.meta_offset + INODE_SIZE + plan.xattrs.len() as u64;
    }
    pad(out, meta_end - cursor)?;

    // The data area. Blocks were handed out in plan order, so writing them in plan order
    // writes them in address order.
    for plan in &planner.plans {
        if plan.blocks == 0 {
            continue;
        }
        let body = match plan.body {
            Body::Empty | Body::Behind(_) => &[][..],
            Body::Bytes(bytes) => bytes,
            Body::Dir(index) => &planner.dirs[index].bytes,
        };
        out.write_all(body)?;
        pad(out, u64::from(plan.blocks) * BLOCK_SIZE - body.len() as u64)?;
    }

    out.seek(SeekFrom::Start(SUPER_OFFSET))?;
    out.write_all(&superblock(planner, total))?;
    out.flush()
}

//--------------------------------------------------------------------------------------------------
// Planning
//--------------------------------------------------------------------------------------------------

/// Where one inode goes and what it holds.
struct Plan<'a> {
    /// Byte offset of the inode within the image.
    meta_offset: u64,
    nid: u64,
    mode: u16,
    uid: u32,
    gid: u32,
    mtime: Timestamp,
    nlink: u32,
    ino: u32,
    /// `i_u` for a device, which is the only inode whose `i_u` is not an address.
    rdev: u32,
    size: u64,
    /// First data block, counted from the start of the data area.
    first_block: u32,
    blocks: u32,
    xattrs: Vec<u8>,
    xattr_icount: u16,
    body: Body<'a>,
}

/// Where an inode's bytes come from.
#[derive(Clone, Copy)]
enum Body<'a> {
    /// Nothing to write: a device, a fifo, an empty file.
    Empty,
    /// A file's contents or a symlink's target, held by the tree.
    Bytes(&'a [u8]),
    /// Directory entries, which are serialised once every nid is known.
    Dir(usize),
    /// A file's contents, already somewhere behind the image: the block it starts at,
    /// counted from the image's end.
    Behind(u32),
}

/// A directory, held apart from its [`Plan`] because its contents cannot be built until
/// the whole tree is placed.
struct DirPlan {
    /// `(name, plan)` in the order EROFS wants dirents in, `.` and `..` included.
    entries: Vec<(Vec<u8>, usize)>,
    /// How many of `entries` go in each block.
    groups: Vec<usize>,
    bytes: Vec<u8>,
}

/// The tree as the planner reads it: each entry with its name and, for a directory, the
/// entries in it.
struct Walked<'a, D> {
    entries: &'a [(Vec<u8>, Inode, &'a Entry<D>)],
    /// For each entry, the indices of its children, by name.
    children: Vec<Vec<usize>>,
    /// How many paths point at each inode, so a hard link gets the right `i_nlink`.
    links: HashMap<Inode, u32>,
}

impl<'a, D> Walked<'a, D> {
    /// `entries` is what [`FsTree::walk`](crate::FsTree::walk) answers: each directory before what is in it,
    /// siblings by name. So a parent is always already seen, and its children arrive in
    /// order.
    fn new(entries: &'a [(Vec<u8>, Inode, &'a Entry<D>)]) -> Walked<'a, D> {
        let mut children = vec![Vec::new(); entries.len()];
        let mut dirs: HashMap<&[u8], usize> = HashMap::new();
        let mut links = HashMap::new();

        for (index, (path, inode, entry)) in entries.iter().enumerate() {
            if index > 0 {
                // The directory a path is in: everything before its last `/`, or the root's
                // empty path.
                let parent = match path.iter().rposition(|byte| *byte == b'/') {
                    Some(slash) => &path[..slash],
                    None => &[],
                };
                children[dirs[parent]].push(index);
            }
            if matches!(entry, Entry::Directory { .. }) {
                dirs.insert(path, index);
            }
            *links.entry(*inode).or_insert(0) += 1;
        }

        Walked {
            entries,
            children,
            links,
        }
    }

    /// An entry's own name: its path past the directory holding it.
    fn name(&self, index: usize) -> &'a [u8] {
        let path = &self.entries[index].0;
        match path.iter().rposition(|byte| *byte == b'/') {
            Some(slash) => &path[slash + 1..],
            None => path,
        }
    }
}

struct Planner<'a> {
    plans: Vec<Plan<'a>>,
    dirs: Vec<DirPlan>,
    /// Inodes already placed, so the second path to one reuses it rather than copying it.
    placed: HashMap<Inode, usize>,
    /// Running offset in the metadata area.
    meta: u64,
    /// Running block in the data area.
    block: u32,
    /// The images behind this one, in the order they follow it, and the block each starts
    /// at, counted from this one's end.
    behind: Vec<Arc<Path>>,
    starts: HashMap<Arc<Path>, u64>,
    /// How many blocks those images come to.
    behind_blocks: u64,
}

impl<'a> Planner<'a> {
    fn plan<D: FileData>(entries: &'a [(Vec<u8>, Inode, &'a Entry<D>)]) -> Result<Planner<'a>> {
        let walked = Walked::new(entries);
        let mut planner = Planner {
            plans: Vec::new(),
            dirs: Vec::new(),
            placed: HashMap::new(),
            meta: META_BLOCK * BLOCK_SIZE,
            block: 0,
            behind: Vec::new(),
            starts: HashMap::new(),
            behind_blocks: 0,
        };

        // The root's parent is the root, which is plan 0 -- the first thing placed.
        planner.plan_entry(&walked, 0, 0)?;
        planner.pack();

        Ok(planner)
    }

    /// How long the image is: the metadata, rounded up to a block, and the data area
    /// behind it.
    fn blocks(&self) -> u32 {
        (align_block(self.meta) / BLOCK_SIZE) as u32 + self.block
    }

    /// Decide where one inode goes.
    fn place(
        &mut self,
        mode: u16,
        meta: &Meta,
        size: u64,
        rdev: u32,
        nlink: u32,
        body: Body<'a>,
    ) -> Result<usize> {
        let (xattrs, xattr_icount) = encode_xattrs(&meta.xattrs);

        // The inode and its xattrs are read as one, so they may not straddle a block.
        let header = INODE_SIZE + xattrs.len() as u64;
        if header > BLOCK_SIZE {
            return Err(Error::Unwritable(
                "EROFS image: an inode's xattrs do not fit beside it".to_string(),
            ));
        }
        if self.meta % BLOCK_SIZE + header > BLOCK_SIZE {
            self.meta = align_block(self.meta);
        }

        let meta_offset = self.meta;
        let blocks = match body {
            Body::Empty | Body::Behind(_) => 0,
            Body::Bytes(_) | Body::Dir(_) => size.div_ceil(BLOCK_SIZE) as u32,
        };
        let first_block = self.block;
        self.block += blocks;
        self.meta = (meta_offset + header).div_ceil(SLOT_SIZE) * SLOT_SIZE;

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
            first_block,
            blocks,
            xattrs,
            xattr_icount,
            body,
        });

        Ok(index)
    }

    fn plan_entry<D: FileData>(
        &mut self,
        walked: &Walked<'a, D>,
        index: usize,
        parent: usize,
    ) -> Result<usize> {
        let (_, inode, entry) = &walked.entries[index];
        let entry: &'a Entry<D> = entry;

        match entry {
            Entry::Directory { meta, opaque } => {
                self.plan_dir(walked, index, parent, meta, *opaque)
            }

            Entry::File { meta, data } => {
                // The second path to a hard-linked inode is a dirent, not an inode.
                if let Some(placed) = self.placed.get(inode) {
                    return Ok(*placed);
                }
                let nlink = walked.links[inode];
                let (size, body) = match data.contents() {
                    Contents::Bytes(bytes) => (bytes.len() as u64, Body::Bytes(bytes)),
                    Contents::Span(span) => {
                        let start = self.behind(&span.path)?;
                        (
                            span.len,
                            Body::Behind((start + u64::from(span.block)) as u32),
                        )
                    }
                };
                let plan =
                    self.place(S_IFREG | (meta.mode & 0o7777), meta, size, 0, nlink, body)?;
                self.placed.insert(*inode, plan);
                Ok(plan)
            }

            Entry::Symlink { meta, target } => self.place(
                S_IFLNK | (meta.mode & 0o7777),
                meta,
                target.len() as u64,
                0,
                1,
                Body::Bytes(target),
            ),

            Entry::Device {
                meta,
                block,
                major,
                minor,
            } => {
                let kind = if *block { S_IFBLK } else { S_IFCHR };
                self.place(
                    kind | (meta.mode & 0o7777),
                    meta,
                    0,
                    encode_dev(*major, *minor),
                    1,
                    Body::Empty,
                )
            }

            Entry::Fifo(meta) => {
                self.place(S_IFIFO | (meta.mode & 0o7777), meta, 0, 0, 1, Body::Empty)
            }

            Entry::Socket(meta) => {
                self.place(S_IFSOCK | (meta.mode & 0o7777), meta, 0, 0, 1, Body::Empty)
            }

            // What overlayfs in the guest reads as a deletion: a character device
            // numbered 0:0.
            Entry::Whiteout => self.place(S_IFCHR, &Meta::default(), 0, 0, 1, Body::Empty),
        }
    }

    fn plan_dir<D: FileData>(
        &mut self,
        walked: &Walked<'a, D>,
        index: usize,
        parent: usize,
        meta: &Meta,
        opaque: bool,
    ) -> Result<usize> {
        let children = &walked.children[index];

        // `.` and `..` are ordinary entries on disk and sort among the rest. The kernel
        // bisects a directory by name, so the order is not cosmetic.
        let mut names: Vec<(&[u8], Option<usize>)> = Vec::with_capacity(children.len() + 2);
        names.push((b".", None));
        names.push((b"..", None));
        names.extend(
            children
                .iter()
                .map(|&child| (walked.name(child), Some(child))),
        );
        names.sort_unstable_by(|a, b| a.0.cmp(b.0));

        let (groups, size) = group_dirents(names.iter().map(|(name, _)| *name))?;

        let dir = self.dirs.len();
        self.dirs.push(DirPlan {
            entries: Vec::new(),
            groups,
            bytes: Vec::new(),
        });

        // What overlayfs in the guest reads as "nothing from below shows through here".
        let opaqued;
        let meta = if opaque {
            let mut with = meta.clone();
            with.xattrs
                .insert(b"trusted.overlay.opaque".to_vec(), b"y".to_vec());
            opaqued = with;
            &opaqued
        } else {
            meta
        };

        // A directory is linked to by its parent, by its own `.`, and by the `..` of each
        // directory under it.
        let subdirs = children
            .iter()
            .filter(|&&child| matches!(walked.entries[child].2, Entry::Directory { .. }))
            .count();
        let plan = self.place(
            S_IFDIR | (meta.mode & 0o7777),
            meta,
            size,
            0,
            2 + subdirs as u32,
            Body::Dir(dir),
        )?;
        // The root is its own parent.
        let parent = if index == 0 { plan } else { parent };

        let mut entries = Vec::with_capacity(names.len());
        for (name, child) in names {
            let target = match (name, child) {
                (b".", None) => plan,
                (b"..", None) => parent,
                (_, Some(child)) => self.plan_entry(walked, child, plan)?,
                _ => unreachable!("only . and .. have no entry"),
            };
            entries.push((name.to_vec(), target));
        }
        self.dirs[dir].entries = entries;

        Ok(plan)
    }

    /// Where the image at `path` starts behind this one, in blocks: placed after every
    /// image already there, the first time it is asked for.
    fn behind(&mut self, path: &Arc<Path>) -> Result<u64> {
        if let Some(start) = self.starts.get(path) {
            return Ok(*start);
        }

        // Its length as it is now, which is what a disk made of it will read.
        let len = std::fs::metadata(path)
            .map_err(|e| Error::io(format!("reading {}", path.display()), e))?
            .len();
        if !len.is_multiple_of(BLOCK_SIZE) {
            return Err(Error::Unwritable(format!(
                "EROFS image: {} is {len} bytes, which puts whatever follows it off a block",
                path.display()
            )));
        }

        let start = self.behind_blocks;
        self.behind_blocks += len / BLOCK_SIZE;
        self.behind.push(path.clone());
        self.starts.insert(path.clone(), start);
        Ok(start)
    }

    /// Serialise every directory, which is only possible now that each entry's nid is
    /// settled.
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
/// A block holds its dirents at the front and the names packed behind them, so an entry
/// costs twelve bytes plus its name and blocks fill greedily. Every block but the last is
/// whole; the last is only as long as it needs to be, and the kernel knows where it ends
/// because that is what `i_size` says.
fn group_dirents<'n>(names: impl Iterator<Item = &'n [u8]>) -> Result<(Vec<usize>, u64)> {
    let block = BLOCK_SIZE as usize;
    let mut groups = Vec::new();
    let (mut count, mut used) = (0, 0);

    for name in names {
        if DIRENT_SIZE + name.len() > block {
            return Err(Error::Unwritable(format!(
                "EROFS image: {} is too long to name a directory entry",
                String::from_utf8_lossy(name)
            )));
        }
        if used + DIRENT_SIZE + name.len() > block {
            groups.push(count);
            (count, used) = (0, 0);
        }
        used += DIRENT_SIZE + name.len();
        count += 1;
    }
    groups.push(count);

    let size = (groups.len() as u64 - 1) * BLOCK_SIZE + used as u64;
    Ok((groups, size))
}

fn pack_dirents(dir: &DirPlan, plans: &[Plan]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut index = 0;

    for (block, count) in dir.groups.iter().enumerate() {
        let entries = &dir.entries[index..index + count];
        let names: usize = entries.iter().map(|(name, _)| name.len()).sum();
        let used = count * DIRENT_SIZE + names;

        // Only the last block is short. The others are padded, and the kernel takes a
        // name to end at the first zero byte, so the padding reads as nothing.
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
/// A name is stored as a prefix index and the rest, so a name with a prefix EROFS has no
/// index for cannot be written at all. Those are dropped rather than refused: a layer
/// that carries one is still a layer worth booting, and the alternative is a pull that
/// fails on an image `podman` would have run.
fn encode_xattrs(xattrs: &BTreeMap<Vec<u8>, Vec<u8>>) -> (Vec<u8>, u16) {
    let storable: Vec<(u8, &[u8], &[u8])> = xattrs
        .iter()
        .filter_map(|(name, value)| {
            let (index, suffix) = xattr_prefix(name)?;
            (suffix.len() <= u8::MAX as usize && value.len() <= u16::MAX as usize).then_some((
                index,
                suffix,
                value.as_slice(),
            ))
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

fn inode(plan: &Plan, data_start: u32, behind_start: u32) -> [u8; INODE_SIZE as usize] {
    let mut inode = [0u8; INODE_SIZE as usize];

    // `i_u` is a device number for a device and a data address for everything else. A
    // whiteout is a character device numbered 0:0, so this has to be by type and not by
    // whether the number looks empty. Behind the image, the block is counted from its end
    // -- and fits, the writer having checked the furthest one does.
    let u = match (plan.mode & S_IFMT, plan.body) {
        (S_IFCHR | S_IFBLK, _) => plan.rdev,
        (_, Body::Behind(block)) => behind_start + block,
        _ => data_start + plan.first_block,
    };

    inode[0..2].copy_from_slice(&(INODE_VERSION_EXTENDED | (LAYOUT_FLAT_PLAIN << 1)).to_le_bytes());
    inode[2..4].copy_from_slice(&plan.xattr_icount.to_le_bytes());
    inode[4..6].copy_from_slice(&plan.mode.to_le_bytes());
    inode[8..16].copy_from_slice(&plan.size.to_le_bytes());
    inode[16..20].copy_from_slice(&u.to_le_bytes());
    inode[20..24].copy_from_slice(&plan.ino.to_le_bytes());
    inode[24..28].copy_from_slice(&plan.uid.to_le_bytes());
    inode[28..32].copy_from_slice(&plan.gid.to_le_bytes());
    inode[32..40].copy_from_slice(&plan.mtime.secs.to_le_bytes());
    inode[40..44].copy_from_slice(&plan.mtime.nanos.to_le_bytes());
    inode[44..48].copy_from_slice(&plan.nlink.to_le_bytes());

    inode
}

fn superblock(planner: &Planner, total: u32) -> [u8; SUPER_SIZE] {
    let mut sb = [0u8; SUPER_SIZE];

    sb[0x00..0x04].copy_from_slice(&SUPER_MAGIC.to_le_bytes());
    // 0x04..0x08 is the checksum, which is over a superblock that says zero here.
    sb[0x08..0x0C].copy_from_slice(&FEATURE_COMPAT_SB_CHKSUM.to_le_bytes());
    sb[0x0C] = BLOCK_BITS;
    // The root is the first inode placed, so its nid is 0 and the 16 bits the superblock
    // gives it are never in question.
    sb[0x0E..0x10].copy_from_slice(&(planner.plans[0].nid as u16).to_le_bytes());
    sb[0x10..0x18].copy_from_slice(&(planner.plans.len() as u64).to_le_bytes());
    sb[0x24..0x28].copy_from_slice(&total.to_le_bytes());
    sb[0x28..0x2C].copy_from_slice(&(META_BLOCK as u32).to_le_bytes());

    // The checksum covers the whole of block 0 past the boot sector: the superblock and
    // the zeros after it.
    let mut block = [0u8; (BLOCK_SIZE - SUPER_OFFSET) as usize];
    block[..SUPER_SIZE].copy_from_slice(&sb);
    sb[0x04..0x08].copy_from_slice(&crc32c(0xFFFF_FFFF, &block).to_le_bytes());

    sb
}

//--------------------------------------------------------------------------------------------------
// Writing
//--------------------------------------------------------------------------------------------------

fn pad(out: &mut impl Write, len: u64) -> std::io::Result<()> {
    const ZEROS: [u8; BLOCK_SIZE as usize] = [0; BLOCK_SIZE as usize];

    let mut left = len;
    while left > 0 {
        let want = left.min(ZEROS.len() as u64) as usize;
        out.write_all(&ZEROS[..want])?;
        left -= want as u64;
    }
    Ok(())
}
