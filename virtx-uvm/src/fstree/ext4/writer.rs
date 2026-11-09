//! Writing an [`FsTree`] as a new ext4 image.
//!
//! # What is written
//!
//! The smallest ext4 a Linux kernel mounts read-write and `e2fsck` passes clean, and what a
//! guest writing to it needs: extents, file types in directory entries, 256-byte inodes, a
//! journal, and `dir_index`. No checksums -- the image is made once, on the host, and
//! handed to one guest.
//!
//! The journal is so that a guest killed midway leaves what its kernel committed and
//! nothing half done, which the reader replays. It is written empty, and costs nothing on
//! the host until the guest writes it.
//!
//! `dir_index` is so that the guest's kernel indexes a directory as it grows past a block,
//! and finds a name in it without reading it all. A directory written here is linear on
//! disk, which reads the same with the feature on; the kernel indexes only one that grows
//! from a single block, so one written longer stays linear.
//!
//! # The shape of the image
//!
//! ```text
//! group 0            superblock in block 0, descriptors from block 1, then as below
//! every group        block bitmap, inode bitmap, inode table, then data
//! ```
//!
//! The journal is inode 8's, and its blocks are handed out right after the root's.
//!
//! `sparse_super2` with no backup groups keeps the superblock and descriptors in group 0
//! alone, so every other group starts with its bitmaps. Blocks are handed out in order
//! from the first free one, inode by inode, skipping each group's metadata.
//!
//! # What is not written
//!
//! What the image holds and the tree does not -- an unused inode, a block nothing is in --
//! is zeros, and is not written at all. So the image costs what the tree holds and not
//! the `size` it is formatted to, on a host that keeps files sparse, and `out` has to read
//! as zeros wherever this does not write: a new file, or an empty buffer.

use std::collections::HashMap;
use std::io::{self, Seek, SeekFrom, Write};

use crate::fstree::{Entry, Error, FsTree, Inode, Meta, Result, Timestamp};

/// A directory entry as it is laid out: its name, its inode number and its file type.
type DirEntry<'a> = (&'a [u8], u32, u8);

const BLOCK: u64 = 4096;
/// As many blocks as one block of bitmap has bits for.
const BLOCKS_PER_GROUP: u64 = 8 * BLOCK;
const INODES_PER_GROUP: u32 = 8192;
const INODE_SIZE: u64 = 256;
const ITABLE_BLOCKS: u64 = INODES_PER_GROUP as u64 * INODE_SIZE / BLOCK;
/// A group descriptor without the `64bit` feature.
const DESC_SIZE: u64 = 32;

/// Inodes 1 to 10 are the filesystem's own. Of them only the root and the journal are used
/// here.
const ROOT: u32 = 2;
const JOURNAL: u32 = 8;
const FIRST_INO: u32 = 11;

/// What every inode carries past the 128 bytes of the original layout: the high halves of
/// its timestamps, among other things.
const EXTRA_ISIZE: u16 = 32;

/// How many extents an inode holds itself, and how many fit in a block of them.
const INODE_EXTENTS: usize = 4;
const LEAF_EXTENTS: usize = ((BLOCK - 12) / 12) as usize;

/// The type bits of `i_mode`.
const S_IFREG: u16 = 0o100000;
const S_IFDIR: u16 = 0o040000;
const S_IFLNK: u16 = 0o120000;
const S_IFCHR: u16 = 0o020000;
const S_IFBLK: u16 = 0o060000;
const S_IFIFO: u16 = 0o010000;
const S_IFSOCK: u16 = 0o140000;

/// Directory entry file types. Not the same numbering as `S_IF*`.
const FT_REG: u8 = 1;
const FT_DIR: u8 = 2;
const FT_CHRDEV: u8 = 3;
const FT_BLKDEV: u8 = 4;
const FT_FIFO: u8 = 5;
const FT_SOCK: u8 = 6;
const FT_SYMLINK: u8 = 7;

const EXTENTS_FL: u32 = 0x8_0000;
const EXTENT_MAGIC: u16 = 0xF30A;
const XATTR_MAGIC: u32 = 0xEA02_0000;

const COMPAT_HAS_JOURNAL: u32 = 0x4;
const COMPAT_EXT_ATTR: u32 = 0x8;
const COMPAT_DIR_INDEX: u32 = 0x20;
const COMPAT_SPARSE_SUPER2: u32 = 0x200;
const INCOMPAT_FILETYPE: u32 = 0x2;
const INCOMPAT_EXTENTS: u32 = 0x40;
const RO_COMPAT_LARGE_FILE: u32 = 0x2;
const RO_COMPAT_EXTRA_ISIZE: u32 = 0x40;

/// What overlayfs in the guest reads as "nothing from below shows through here".
const OPAQUE: &[u8] = b"trusted.overlay.opaque";

/// What the kernel hashes names in an indexed directory with: half MD4, from a seed of the
/// filesystem's own. Fixed, so the same tree makes the same image.
const HASH_HALF_MD4: u8 = 1;
const HASH_SEED: [u8; 16] = *b"virtx-uvm ext4\0\0";
/// `s_flags`: names hashed as unsigned chars, which says how the kernel hashes them
/// whatever the guest's architecture.
const FLAGS_UNSIGNED_HASH: u32 = 0x2;

/// jbd2's: the magic every journal block starts with, and a version 2 superblock's type.
const JOURNAL_MAGIC: u32 = 0xC03B_3998;
const JOURNAL_SUPERBLOCK_V2: u32 = 4;

/// The directory `e2fsck` puts what it finds unattached in, made when the tree has none.
static LOST_FOUND: Meta = Meta {
    mode: 0o700,
    uid: 0,
    gid: 0,
    atime: Timestamp::ZERO,
    mtime: Timestamp::ZERO,
    ctime: Timestamp::ZERO,
    crtime: Timestamp::ZERO,
    xattrs: std::collections::BTreeMap::new(),
};

/// The journal's, which nothing but the kernel opens.
static JOURNAL_META: Meta = Meta {
    mode: 0o600,
    uid: 0,
    gid: 0,
    atime: Timestamp::ZERO,
    mtime: Timestamp::ZERO,
    ctime: Timestamp::ZERO,
    crtime: Timestamp::ZERO,
    xattrs: std::collections::BTreeMap::new(),
};

/// What a whiteout has, which is nothing of its own.
static NO_META: Meta = Meta {
    mode: 0,
    uid: 0,
    gid: 0,
    atime: Timestamp::ZERO,
    mtime: Timestamp::ZERO,
    ctime: Timestamp::ZERO,
    crtime: Timestamp::ZERO,
    xattrs: std::collections::BTreeMap::new(),
};

/// Where an inode's bytes come from.
enum Content<'a> {
    /// Nothing at all: a fifo, a device, an empty file.
    Empty,
    /// A file's contents, or a symlink's target too long to keep in the inode.
    Bytes(&'a [u8]),
    /// A directory's entries, packed into blocks.
    Dir(Vec<u8>),
    /// A symlink's target, short enough to sit where the inode would map its blocks.
    Inline(&'a [u8]),
    /// The journal's superblock, in the first of the journal's blocks; the rest are its
    /// log, zeros until the guest writes it.
    Journal(Vec<u8>),
}

/// One inode, and where everything of it goes.
struct Node<'a> {
    ino: u32,
    /// The whole of `i_mode`, type bits included.
    mode: u16,
    meta: &'a Meta,
    /// For a directory: whether nothing from the layers below shows through it.
    opaque: bool,
    nlink: u32,
    size: u64,
    /// `i_block` for a device: its number, in the kernel's encoding.
    rdev: (u32, u32),
    content: Content<'a>,
    /// The blocks `content` is in, as runs of `(logical, first, count)`: `count` blocks of
    /// it from block `logical` on, at block `first` of the image. A block of a file that is
    /// all zeros is in none of them, and reads as zeros for being a hole.
    runs: Vec<(u64, u64, u64)>,
    /// The blocks holding the extents, when there are more than the inode holds itself.
    leaves: Vec<u64>,
    /// The block holding the inode's xattrs, and what goes in it.
    xattrs: Option<(u64, Vec<u8>)>,
}

impl Node<'_> {
    /// How many blocks of the image the inode owns, which is what `i_blocks` counts in
    /// 512-byte sectors.
    fn blocks(&self) -> u64 {
        self.runs.iter().map(|(.., count)| count).sum::<u64>()
            + self.leaves.len() as u64
            + u64::from(self.xattrs.is_some())
    }

    /// The extents mapping the inode's blocks, as they are stored: logical block, length,
    /// physical block.
    fn extents(&self) -> Vec<[u8; 12]> {
        self.runs
            .iter()
            .map(|(logical, first, count)| {
                let mut extent = [0u8; 12];
                extent[0..4].copy_from_slice(&(*logical as u32).to_le_bytes());
                extent[4..6].copy_from_slice(&(*count as u16).to_le_bytes());
                extent[6..8].copy_from_slice(&((first >> 32) as u16).to_le_bytes());
                extent[8..12].copy_from_slice(&(*first as u32).to_le_bytes());
                extent
            })
            .collect()
    }
}

/// An image, planned: how long it is, and every inode in it with its blocks.
pub(super) struct Plan<'a> {
    blocks: u64,
    groups: u64,
    gdt_blocks: u64,
    /// By inode number.
    nodes: Vec<Node<'a>>,
}

impl<'a> Plan<'a> {
    /// Lay `tree` out on an image of `size` bytes.
    pub(super) fn new(tree: &'a FsTree, size: u64) -> Result<Plan<'a>> {
        // A group needs room for its own bitmaps and inode table and a block besides. A
        // last group without that much is left off, as `mke2fs` leaves it.
        let mut blocks = size / BLOCK;
        if !blocks.is_multiple_of(BLOCKS_PER_GROUP)
            && blocks % BLOCKS_PER_GROUP <= 2 + ITABLE_BLOCKS
        {
            blocks -= blocks % BLOCKS_PER_GROUP;
        }
        let groups = blocks.div_ceil(BLOCKS_PER_GROUP);
        let gdt_blocks = (groups * DESC_SIZE).div_ceil(BLOCK);
        if groups == 0 || blocks <= 1 + gdt_blocks + 2 + ITABLE_BLOCKS {
            return Err(Error::Unwritable(format!(
                "ext4 image: {size} bytes is too small to hold a filesystem"
            )));
        }

        // The first block of each group nothing of the filesystem's own is in.
        let first_free = |group: u64| {
            group * BLOCKS_PER_GROUP
                + if group == 0 { 1 + gdt_blocks } else { 0 }
                + 2
                + ITABLE_BLOCKS
        };
        let mut next = first_free(0);
        let mut take = |mut count: u64| -> Result<Vec<(u64, u64)>> {
            let mut runs = Vec::new();
            while count > 0 {
                if next >= blocks {
                    return Err(Error::Unwritable(format!(
                        "ext4 image: the tree does not fit in {size} bytes"
                    )));
                }
                let group = next / BLOCKS_PER_GROUP;
                next = next.max(first_free(group));
                let end = ((group + 1) * BLOCKS_PER_GROUP).min(blocks);
                let len = count.min(end - next);
                runs.push((next, len));
                next += len;
                count -= len;
            }
            Ok(runs)
        };

        // As `mke2fs` sizes a journal, up to the 32 MiB a busy guest has room in.
        let journal = match blocks {
            0..32768 => 1024,
            32768..262144 => 4096,
            _ => 8192,
        };
        let mut nodes = place(tree)?;
        nodes.push(Node {
            ino: JOURNAL,
            mode: S_IFREG | JOURNAL_META.mode,
            meta: &JOURNAL_META,
            opaque: false,
            nlink: 1,
            size: journal * BLOCK,
            rdev: (0, 0),
            content: Content::Journal(journal_superblock(journal as u32)),
            runs: Vec::new(),
            leaves: Vec::new(),
            xattrs: None,
        });
        nodes.sort_by_key(|node| node.ino);

        for node in &mut nodes {
            let bytes: &[u8] = match &node.content {
                Content::Bytes(bytes) => bytes,
                Content::Dir(bytes) => bytes,
                Content::Empty | Content::Inline(_) | Content::Journal(_) => &[],
            };
            // The stretches of blocks that hold something, as `(logical, count)`. Only a
            // file has holes: a directory or a symlink is read whole, block by block. The
            // journal is held whole, for the kernel to write its log into.
            let mut held: Vec<(u64, u64)> = match node.content {
                Content::Journal(_) => vec![(0, node.size / BLOCK)],
                _ => Vec::new(),
            };
            for (index, block) in bytes.chunks(BLOCK as usize).enumerate() {
                let index = index as u64;
                let hole = node.mode & 0o170000 == S_IFREG && block.iter().all(|byte| *byte == 0);
                match held.last_mut() {
                    _ if hole => {}
                    Some((logical, count)) if *logical + *count == index => *count += 1,
                    _ => held.push((index, 1)),
                }
            }
            // A file that is more holes than one level of extents can map between is
            // written whole instead.
            if held.len() > LEAF_EXTENTS {
                held = vec![(0, bytes.len().div_ceil(BLOCK as usize) as u64)];
            }
            node.runs.clear();
            for (mut logical, count) in held {
                for (first, len) in take(count)? {
                    node.runs.push((logical, first, len));
                    logical += len;
                }
            }
            if node.runs.len() > INODE_EXTENTS {
                let leaves = node.runs.len().div_ceil(LEAF_EXTENTS);
                if leaves > INODE_EXTENTS {
                    return Err(Error::Unwritable(format!(
                        "ext4 image: inode {} is in more pieces than one level of extents maps",
                        node.ino
                    )));
                }
                node.leaves = take(leaves as u64)?
                    .into_iter()
                    .flat_map(|(first, count)| first..first + count)
                    .collect();
            }

            // Carried by the tree as a flag, and by the image as what overlayfs reads.
            let mut xattrs = node.meta.xattrs.clone();
            if node.opaque {
                xattrs.insert(OPAQUE.to_vec(), b"y".to_vec());
            }
            if let Some(block) = encode_xattrs(&xattrs, node.ino)? {
                node.xattrs = Some((take(1)?[0].0, block));
            }
        }

        let inodes = groups * u64::from(INODES_PER_GROUP);
        let last = nodes.last().map_or(FIRST_INO, |node| node.ino);
        if u64::from(last) > inodes {
            return Err(Error::Unwritable(format!(
                "ext4 image: the tree has {last} inodes, and {size} bytes holds {inodes}"
            )));
        }

        Ok(Plan {
            blocks,
            groups,
            gdt_blocks,
            nodes,
        })
    }

    /// Put the planned image down on `out`.
    pub(super) fn emit(&self, out: &mut (impl Write + Seek)) -> io::Result<()> {
        // The end first, so the image is as long as it says even where nothing is written
        // near it -- and before the data, which may be in the last block.
        out.seek(SeekFrom::Start(self.blocks * BLOCK - 1))?;
        out.write_all(&[0])?;

        // Which blocks and inodes are taken, group by group.
        let mut block_bitmaps = vec![vec![0u8; BLOCK as usize]; self.groups as usize];
        let mut inode_bitmaps = vec![vec![0u8; BLOCK as usize]; self.groups as usize];
        let mut dirs = vec![0u32; self.groups as usize];
        let mut mark = |block: u64| {
            let within = block % BLOCKS_PER_GROUP;
            block_bitmaps[(block / BLOCKS_PER_GROUP) as usize][(within / 8) as usize] |=
                1 << (within % 8);
        };
        for group in 0..self.groups {
            let start = group * BLOCKS_PER_GROUP;
            let own = if group == 0 { 1 + self.gdt_blocks } else { 0 } + 2 + ITABLE_BLOCKS;
            (start..start + own).for_each(&mut mark);
        }
        for node in &self.nodes {
            for (_, first, count) in &node.runs {
                (*first..first + count).for_each(&mut mark);
            }
            node.leaves.iter().copied().for_each(&mut mark);
            if let Some((block, _)) = &node.xattrs {
                mark(*block);
            }
        }
        // Past the end of a short last group is not there to be free.
        for block in self.blocks..self.groups * BLOCKS_PER_GROUP {
            mark(block);
        }

        let used_inodes = (1..FIRST_INO).chain(self.nodes.iter().map(|node| node.ino));
        for ino in used_inodes {
            let within = (ino - 1) % INODES_PER_GROUP;
            inode_bitmaps[((ino - 1) / INODES_PER_GROUP) as usize][(within / 8) as usize] |=
                1 << (within % 8);
        }
        for bitmap in &mut inode_bitmaps {
            bitmap[(INODES_PER_GROUP / 8) as usize..].fill(0xFF);
        }
        for node in &self.nodes {
            if node.mode & 0o170000 == S_IFDIR {
                dirs[((node.ino - 1) / INODES_PER_GROUP) as usize] += 1;
            }
        }

        let free = |bitmap: &[u8], bits: u64| -> u64 {
            bits - bitmap
                .iter()
                .map(|byte| u64::from(byte.count_ones()))
                .sum::<u64>()
        };
        let mut descriptors = vec![0u8; (self.gdt_blocks * BLOCK) as usize];
        let (mut free_blocks, mut free_inodes) = (0, 0);
        for group in 0..self.groups as usize {
            let start = group as u64 * BLOCKS_PER_GROUP;
            let bitmap = start + if group == 0 { 1 + self.gdt_blocks } else { 0 };
            let group_free_blocks = free(&block_bitmaps[group], BLOCKS_PER_GROUP);
            let group_free_inodes = free(&inode_bitmaps[group], 8 * BLOCK);
            free_blocks += group_free_blocks;
            free_inodes += group_free_inodes;

            let at = group * DESC_SIZE as usize;
            let desc = &mut descriptors[at..at + DESC_SIZE as usize];
            desc[0x00..0x04].copy_from_slice(&(bitmap as u32).to_le_bytes());
            desc[0x04..0x08].copy_from_slice(&(bitmap as u32 + 1).to_le_bytes());
            desc[0x08..0x0C].copy_from_slice(&(bitmap as u32 + 2).to_le_bytes());
            desc[0x0C..0x0E].copy_from_slice(&(group_free_blocks as u16).to_le_bytes());
            desc[0x0E..0x10].copy_from_slice(&(group_free_inodes as u16).to_le_bytes());
            desc[0x10..0x12].copy_from_slice(&(dirs[group] as u16).to_le_bytes());

            out.seek(SeekFrom::Start(bitmap * BLOCK))?;
            out.write_all(&block_bitmaps[group])?;
            out.write_all(&inode_bitmaps[group])?;
        }

        let mut block0 = vec![0u8; BLOCK as usize];
        block0[1024..2048].copy_from_slice(&self.superblock(free_blocks, free_inodes));
        out.seek(SeekFrom::Start(0))?;
        out.write_all(&block0)?;
        out.write_all(&descriptors)?;

        // The inode tables, as far into each as an inode is used.
        let mut tables: HashMap<u64, Vec<u8>> = HashMap::new();
        for node in &self.nodes {
            let group = u64::from((node.ino - 1) / INODES_PER_GROUP);
            let at = (u64::from((node.ino - 1) % INODES_PER_GROUP) * INODE_SIZE) as usize;
            let table = tables.entry(group).or_default();
            if table.len() < at + INODE_SIZE as usize {
                table.resize(
                    (at + INODE_SIZE as usize).next_multiple_of(BLOCK as usize),
                    0,
                );
            }
            table[at..at + INODE_SIZE as usize].copy_from_slice(&inode(node));
        }
        for (group, table) in tables {
            let start = group * BLOCKS_PER_GROUP + if group == 0 { 1 + self.gdt_blocks } else { 0 };
            out.seek(SeekFrom::Start((start + 2) * BLOCK))?;
            out.write_all(&table)?;
        }

        for node in &self.nodes {
            let bytes: &[u8] = match &node.content {
                Content::Bytes(bytes) => bytes,
                Content::Dir(bytes) | Content::Journal(bytes) => bytes,
                Content::Empty | Content::Inline(_) => &[],
            };
            for (logical, first, count) in &node.runs {
                let start = (logical * BLOCK) as usize;
                let len = (count * BLOCK) as usize;
                let here = bytes
                    .get(start..)
                    .map_or(&[][..], |rest| &rest[..len.min(rest.len())]);
                out.seek(SeekFrom::Start(first * BLOCK))?;
                out.write_all(here)?;
                // To the end of the last block written to, so nothing that was there before
                // shows through its tail. Past it, `out` reads as zeros.
                out.write_all(&vec![
                    0u8;
                    here.len().next_multiple_of(BLOCK as usize)
                        - here.len()
                ])?;
            }

            if !node.leaves.is_empty() {
                for (leaf, extents) in node.leaves.iter().zip(node.extents().chunks(LEAF_EXTENTS)) {
                    let mut block = vec![0u8; BLOCK as usize];
                    block[..12].copy_from_slice(&extent_header(extents.len(), LEAF_EXTENTS, 0));
                    for (index, extent) in extents.iter().enumerate() {
                        block[12 + 12 * index..24 + 12 * index].copy_from_slice(extent);
                    }
                    out.seek(SeekFrom::Start(leaf * BLOCK))?;
                    out.write_all(&block)?;
                }
            }

            if let Some((block, bytes)) = &node.xattrs {
                out.seek(SeekFrom::Start(block * BLOCK))?;
                out.write_all(bytes)?;
            }
        }

        out.flush()
    }

    fn superblock(&self, free_blocks: u64, free_inodes: u64) -> [u8; 1024] {
        let mut sb = [0u8; 1024];
        let inodes = self.groups as u32 * INODES_PER_GROUP;
        let put = |sb: &mut [u8; 1024], at: usize, bytes: &[u8]| {
            sb[at..at + bytes.len()].copy_from_slice(bytes)
        };

        put(&mut sb, 0x00, &inodes.to_le_bytes());
        put(&mut sb, 0x04, &(self.blocks as u32).to_le_bytes());
        put(&mut sb, 0x0C, &(free_blocks as u32).to_le_bytes());
        put(&mut sb, 0x10, &(free_inodes as u32).to_le_bytes());
        // s_first_data_block is 0 with 4096-byte blocks: block 0 holds the superblock.
        put(&mut sb, 0x18, &2u32.to_le_bytes()); // s_log_block_size: 1024 << 2
        put(&mut sb, 0x1C, &2u32.to_le_bytes()); // s_log_cluster_size
        put(&mut sb, 0x20, &(BLOCKS_PER_GROUP as u32).to_le_bytes());
        put(&mut sb, 0x24, &(BLOCKS_PER_GROUP as u32).to_le_bytes());
        put(&mut sb, 0x28, &INODES_PER_GROUP.to_le_bytes());
        put(&mut sb, 0x36, &u16::MAX.to_le_bytes()); // s_max_mnt_count: never forces a check
        put(&mut sb, 0x38, &0xEF53u16.to_le_bytes());
        put(&mut sb, 0x3A, &1u16.to_le_bytes()); // s_state: clean
        put(&mut sb, 0x3C, &1u16.to_le_bytes()); // s_errors: continue
        // s_creator_os 0 is Linux, which says how each inode's osd2 is laid out.
        put(&mut sb, 0x4C, &1u32.to_le_bytes()); // s_rev_level: dynamic
        put(&mut sb, 0x54, &FIRST_INO.to_le_bytes());
        put(&mut sb, 0x58, &(INODE_SIZE as u16).to_le_bytes());
        put(
            &mut sb,
            0x5C,
            &(COMPAT_HAS_JOURNAL | COMPAT_EXT_ATTR | COMPAT_DIR_INDEX | COMPAT_SPARSE_SUPER2)
                .to_le_bytes(),
        );
        put(
            &mut sb,
            0x60,
            &(INCOMPAT_FILETYPE | INCOMPAT_EXTENTS).to_le_bytes(),
        );
        put(
            &mut sb,
            0x64,
            &(RO_COMPAT_LARGE_FILE | RO_COMPAT_EXTRA_ISIZE).to_le_bytes(),
        );
        put(&mut sb, 0xE0, &JOURNAL.to_le_bytes()); // s_journal_inum
        put(&mut sb, 0xEC, &HASH_SEED); // s_hash_seed
        put(&mut sb, 0xFC, &[HASH_HALF_MD4]); // s_def_hash_version
        // s_jnl_blocks: the journal inode's block map and size, for `e2fsck` to find the
        // journal by should the inode be lost.
        let journal = self
            .nodes
            .iter()
            .find(|node| node.ino == JOURNAL)
            .expect("every plan has a journal");
        let raw = inode(journal);
        put(&mut sb, 0xFD, &[1]); // s_jnl_backup_type: a copy of the block map
        put(&mut sb, 0x10C, &raw[0x28..0x64]);
        put(&mut sb, 0x148, &raw[0x6C..0x70]);
        put(&mut sb, 0x14C, &raw[0x04..0x08]);
        put(&mut sb, 0x15C, &EXTRA_ISIZE.to_le_bytes()); // s_min_extra_isize
        put(&mut sb, 0x15E, &EXTRA_ISIZE.to_le_bytes()); // s_want_extra_isize
        put(&mut sb, 0x160, &FLAGS_UNSIGNED_HASH.to_le_bytes()); // s_flags
        // s_backup_bgs at 0x24C stays zero: no group but the first holds a superblock.
        sb
    }
}

/// The superblock of a journal `blocks` long: empty, with nothing for a mount to replay.
fn journal_superblock(blocks: u32) -> Vec<u8> {
    let mut sb = vec![0u8; BLOCK as usize];
    let mut put = |at: usize, value: u32| sb[at..at + 4].copy_from_slice(&value.to_be_bytes());
    put(0x00, JOURNAL_MAGIC);
    put(0x04, JOURNAL_SUPERBLOCK_V2);
    put(0x0C, BLOCK as u32); // s_blocksize
    put(0x10, blocks); // s_maxlen
    put(0x14, 1); // s_first: the log, past this block
    put(0x18, 1); // s_sequence: the first transaction's
    // s_start 0 is a log with nothing in it.
    put(0x40, 1); // s_nr_users: the filesystem it is in
    sb
}

/// A directory entry as it is gathered: its name, inode number and file type.

/// Every inode the tree has, numbered, with its directory entries packed: the root as
/// inode 2 and the rest from 11 in the order the tree walks them. A hard link is one more
/// name for an inode already numbered.
fn place(tree: &FsTree) -> Result<Vec<Node<'_>>> {
    let walked = tree.walk();
    // Made unless the tree has something by that name already, whatever it is.
    let has_lost_found = walked.iter().any(|(path, ..)| path == b"lost+found");

    let mut numbers: HashMap<Inode, u32> = HashMap::new();
    let mut next = if has_lost_found {
        FIRST_INO
    } else {
        FIRST_INO + 1
    };
    let mut dirs: HashMap<&[u8], u32> = HashMap::new();
    // Each directory's entries, by its inode number, and its parent's.
    let mut entries: HashMap<u32, Vec<DirEntry<'_>>> = HashMap::new();
    let mut parents: HashMap<u32, u32> = HashMap::new();
    let mut nodes: Vec<Node> = Vec::new();
    let mut at: HashMap<u32, usize> = HashMap::new();

    for (path, id, entry) in &walked {
        let (path, entry): (&[u8], &Entry) = (path, *entry);
        let (parent, name) = match path.iter().rposition(|byte| *byte == b'/') {
            Some(slash) => (&path[..slash], &path[slash + 1..]),
            None => (&b""[..], path),
        };
        let ino = if path.is_empty() {
            ROOT
        } else if let Some(&ino) = numbers.get(id) {
            nodes[at[&ino]].nlink += 1;
            ino
        } else {
            next += 1;
            next - 1
        };

        if !path.is_empty() {
            if name.len() > 255 {
                return Err(Error::Unwritable(format!(
                    "ext4 image: {} is longer than a name can be",
                    String::from_utf8_lossy(path)
                )));
            }
            let parent = dirs[parent];
            let kind = match entry {
                Entry::File { .. } => FT_REG,
                Entry::Directory { .. } => FT_DIR,
                Entry::Symlink { .. } => FT_SYMLINK,
                Entry::Device { block: true, .. } => FT_BLKDEV,
                Entry::Device { block: false, .. } | Entry::Whiteout => FT_CHRDEV,
                Entry::Fifo(_) => FT_FIFO,
                Entry::Socket(_) => FT_SOCK,
            };
            entries.entry(parent).or_default().push((name, ino, kind));
            if let Entry::Directory { .. } = entry {
                parents.insert(ino, parent);
            }
        }
        if let Entry::Directory { .. } = entry {
            dirs.insert(path, ino);
        }
        if numbers.contains_key(id) {
            continue;
        }
        numbers.insert(*id, ino);

        let blank = (0, 0);
        let (mode, meta, size, rdev, content) = match entry {
            Entry::File { meta, data } => (
                S_IFREG | meta.mode,
                meta,
                data.len() as u64,
                blank,
                if data.is_empty() {
                    Content::Empty
                } else {
                    Content::Bytes(data)
                },
            ),
            // Packed below, once every entry in it is known.
            Entry::Directory { meta, .. } => (S_IFDIR | meta.mode, meta, 0, blank, Content::Empty),
            Entry::Symlink { meta, target } => (
                S_IFLNK | 0o777,
                meta,
                target.len() as u64,
                blank,
                if target.len() < 60 {
                    Content::Inline(target)
                } else {
                    Content::Bytes(target)
                },
            ),
            Entry::Device {
                meta,
                block,
                major,
                minor,
            } => (
                if *block { S_IFBLK } else { S_IFCHR } | meta.mode,
                meta,
                0,
                (*major, *minor),
                Content::Empty,
            ),
            Entry::Fifo(meta) => (S_IFIFO | meta.mode, meta, 0, blank, Content::Empty),
            Entry::Socket(meta) => (S_IFSOCK | meta.mode, meta, 0, blank, Content::Empty),
            // What overlayfs deletes with: a character device numbered 0:0.
            Entry::Whiteout => (S_IFCHR, &NO_META, 0, blank, Content::Empty),
        };

        at.insert(ino, nodes.len());
        nodes.push(Node {
            ino,
            mode,
            meta,
            opaque: matches!(entry, Entry::Directory { opaque: true, .. }),
            nlink: 1,
            size,
            rdev,
            content,
            runs: Vec::new(),
            leaves: Vec::new(),
            xattrs: None,
        });
    }

    if !has_lost_found {
        entries
            .entry(ROOT)
            .or_default()
            .push((b"lost+found", FIRST_INO, FT_DIR));
        parents.insert(FIRST_INO, ROOT);
        at.insert(FIRST_INO, nodes.len());
        nodes.push(Node {
            ino: FIRST_INO,
            mode: S_IFDIR | LOST_FOUND.mode,
            meta: &LOST_FOUND,
            opaque: false,
            nlink: 1,
            size: 0,
            rdev: (0, 0),
            content: Content::Empty,
            runs: Vec::new(),
            leaves: Vec::new(),
            xattrs: None,
        });
    }

    for node in &mut nodes {
        if node.mode & 0o170000 != S_IFDIR {
            continue;
        }
        let parent = if node.ino == ROOT {
            ROOT
        } else {
            parents[&node.ino]
        };
        let children = entries.remove(&node.ino).unwrap_or_default();

        // A directory is linked to by its parent, by its own `.`, and by the `..` of each
        // directory under it.
        let subdirs = children.iter().filter(|(.., kind)| *kind == FT_DIR).count();
        node.nlink = 2 + subdirs as u32;

        // Linear blocks of `inode, rec_len, name_len, file_type, name`, each entry padded
        // to four bytes and the last in a block running to the block's end. An entry never
        // straddles two blocks.
        let block = BLOCK as usize;
        let mut packed: Vec<u8> = Vec::new();
        let (mut block_start, mut last) = (0, 0);
        let all = [(&b"."[..], node.ino, FT_DIR), (&b".."[..], parent, FT_DIR)]
            .into_iter()
            .chain(children);
        for (name, ino, kind) in all {
            let len = (8 + name.len()).next_multiple_of(4);
            if packed.len() + len > block_start + block {
                // Stretch the last entry over the block's tail, and start a new block.
                let rec_len = (block_start + block - last) as u16;
                packed[last + 4..last + 6].copy_from_slice(&rec_len.to_le_bytes());
                packed.resize(block_start + block, 0);
                block_start += block;
            }
            last = packed.len();
            packed.extend_from_slice(&ino.to_le_bytes());
            packed.extend_from_slice(&(len as u16).to_le_bytes());
            packed.push(name.len() as u8);
            packed.push(kind);
            packed.extend_from_slice(name);
            packed.resize(last + len, 0);
        }
        let rec_len = (block_start + block - last) as u16;
        packed[last + 4..last + 6].copy_from_slice(&rec_len.to_le_bytes());
        packed.resize(block_start + block, 0);

        node.size = packed.len() as u64;
        node.content = Content::Dir(packed);
    }

    for node in &nodes {
        if node.nlink > 65000 {
            return Err(Error::Unwritable(format!(
                "ext4 image: inode {} has {} links, more than an inode can count",
                node.ino, node.nlink
            )));
        }
    }

    nodes.sort_by_key(|node| node.ino);
    Ok(nodes)
}

/// An inode's xattrs as a block of its own, sorted as the kernel looks them up. `None` for
/// an inode that has none.
///
/// A name is stored as a prefix index and the rest, so a name with a prefix ext4 has no
/// index for cannot be written; those are dropped, as the EROFS writer drops them. A POSIX
/// ACL is stored in ext4's own layout, and not the one every other form carries it in.
fn encode_xattrs(
    xattrs: &std::collections::BTreeMap<Vec<u8>, Vec<u8>>,
    ino: u32,
) -> Result<Option<Vec<u8>>> {
    // The ACLs are whole names, so they are looked for before the `system.` they are under.
    const PREFIXES: [(&[u8], u8); 6] = [
        (b"system.posix_acl_access", 2),
        (b"system.posix_acl_default", 3),
        (b"user.", 1),
        (b"trusted.", 4),
        (b"security.", 6),
        (b"system.", 7),
    ];

    let mut storable: Vec<(u8, &[u8], Vec<u8>)> = Vec::new();
    for (name, value) in xattrs {
        // Where ext4 keeps inline data, which this never writes any of.
        if name == b"system.data" {
            continue;
        }
        let Some((prefix, index)) = PREFIXES.iter().find(|(prefix, _)| name.starts_with(prefix))
        else {
            continue;
        };
        let suffix = &name[prefix.len()..];
        if suffix.len() > 255 {
            continue;
        }
        let value = match index {
            2 | 3 => acl_to_ext4(value).ok_or_else(|| {
                Error::Unwritable(format!(
                    "ext4 image: inode {ino} has a {} that is not an ACL",
                    String::from_utf8_lossy(name)
                ))
            })?,
            _ => value.clone(),
        };
        storable.push((*index, suffix, value));
    }
    if storable.is_empty() {
        return Ok(None);
    }
    storable.sort_by(|a, b| (a.0, a.1.len(), a.1).cmp(&(b.0, b.1.len(), b.1)));

    let mut block = vec![0u8; BLOCK as usize];
    let mut entry_at = 32;
    let mut value_at = BLOCK as usize;
    let mut block_hash = 0u32;
    for (index, suffix, value) in &storable {
        let (index, suffix, value) = (*index, *suffix, value.as_slice());
        let entry_len = (16 + suffix.len()).next_multiple_of(4);
        let value_len = value.len().next_multiple_of(4);
        // Four zero bytes end the entries, and the values are packed down from the end.
        if entry_at + entry_len + 4 > value_at.saturating_sub(value_len) {
            return Err(Error::Unwritable(format!(
                "ext4 image: inode {ino}'s xattrs do not fit in a block"
            )));
        }
        value_at -= value_len;
        block[value_at..value_at + value.len()].copy_from_slice(value);

        // The kernel's `ext4_xattr_hash_entry`: the name a byte at a time, the value a
        // little-endian word at a time.
        let mut hash = 0u32;
        for byte in suffix {
            hash = (hash << 5) ^ (hash >> 27) ^ (*byte as i8 as i32 as u32);
        }
        for word in block[value_at..value_at + value_len].chunks(4) {
            hash = (hash << 16) ^ (hash >> 16) ^ u32::from_le_bytes(word.try_into().unwrap());
        }
        block_hash = (block_hash << 16) ^ (block_hash >> 16) ^ hash;

        let entry = &mut block[entry_at..entry_at + entry_len];
        entry[0] = suffix.len() as u8;
        entry[1] = index;
        // An empty value is nowhere, and says so.
        let offset = if value.is_empty() { 0 } else { value_at as u16 };
        entry[2..4].copy_from_slice(&offset.to_le_bytes());
        entry[8..12].copy_from_slice(&(value.len() as u32).to_le_bytes());
        entry[12..16].copy_from_slice(&hash.to_le_bytes());
        entry[16..16 + suffix.len()].copy_from_slice(suffix);
        entry_at += entry_len;
    }

    block[0..4].copy_from_slice(&XATTR_MAGIC.to_le_bytes());
    block[4..8].copy_from_slice(&1u32.to_le_bytes()); // h_refcount
    block[8..12].copy_from_slice(&1u32.to_le_bytes()); // h_blocks
    block[12..16].copy_from_slice(&block_hash.to_le_bytes());
    Ok(Some(block))
}

/// A POSIX ACL as the kernel hands one out -- version 2, eight bytes an entry -- in the
/// layout ext4 stores it in: version 1, and the entries that name nobody in four bytes,
/// without the id they have no use for. `None` for something that is not an ACL.
fn acl_to_ext4(value: &[u8]) -> Option<Vec<u8>> {
    let (version, entries) = value.split_first_chunk::<4>()?;
    if u32::from_le_bytes(*version) != 2 || entries.len() % 8 != 0 {
        return None;
    }
    let mut out = 1u32.to_le_bytes().to_vec();
    for entry in entries.chunks(8) {
        match u16::from_le_bytes([entry[0], entry[1]]) {
            // The owner, the owning group, the mask, and everyone else.
            0x01 | 0x04 | 0x10 | 0x20 => out.extend_from_slice(&entry[..4]),
            // A named user or group.
            0x02 | 0x08 => out.extend_from_slice(entry),
            _ => return None,
        }
    }
    Some(out)
}

fn extent_header(entries: usize, max: usize, depth: u16) -> [u8; 12] {
    let mut header = [0u8; 12];
    header[0..2].copy_from_slice(&EXTENT_MAGIC.to_le_bytes());
    header[2..4].copy_from_slice(&(entries as u16).to_le_bytes());
    header[4..6].copy_from_slice(&(max as u16).to_le_bytes());
    header[6..8].copy_from_slice(&depth.to_le_bytes());
    header
}

fn inode(node: &Node) -> [u8; INODE_SIZE as usize] {
    let mut inode = [0u8; INODE_SIZE as usize];
    let meta = node.meta;
    let put = |inode: &mut [u8; INODE_SIZE as usize], at: usize, bytes: &[u8]| {
        inode[at..at + bytes.len()].copy_from_slice(bytes)
    };

    // The kernel's `ext4_encode_extra_time`: the seconds as a signed 32 bits, and in each
    // `_extra` two more bits of epoch beneath the nanoseconds.
    let time = |time: Timestamp| {
        let seconds = (time.secs as i32).to_le_bytes();
        let epoch = ((time.secs - i64::from(time.secs as i32)) >> 32) as u32 & 3;
        (seconds, (time.nanos << 2 | epoch).to_le_bytes())
    };
    let (atime, atime_extra) = time(meta.atime);
    let (ctime, ctime_extra) = time(meta.ctime);
    let (mtime, mtime_extra) = time(meta.mtime);
    let (crtime, crtime_extra) = time(meta.crtime);

    put(&mut inode, 0x00, &node.mode.to_le_bytes());
    put(&mut inode, 0x02, &(meta.uid as u16).to_le_bytes());
    put(&mut inode, 0x04, &(node.size as u32).to_le_bytes());
    put(&mut inode, 0x08, &atime);
    put(&mut inode, 0x0C, &ctime);
    put(&mut inode, 0x10, &mtime);
    put(&mut inode, 0x18, &(meta.gid as u16).to_le_bytes());
    put(&mut inode, 0x1A, &(node.nlink as u16).to_le_bytes());
    put(
        &mut inode,
        0x1C,
        &((node.blocks() * (BLOCK / 512)) as u32).to_le_bytes(),
    );

    match &node.content {
        Content::Inline(target) => put(&mut inode, 0x28, target),
        _ if node.mode & 0o170000 == S_IFCHR || node.mode & 0o170000 == S_IFBLK => {
            // The kernel's own choice between its two encodings: the old one where both
            // numbers fit a byte, the new one in the second word where they do not.
            let (major, minor) = node.rdev;
            if major < 256 && minor < 256 {
                put(&mut inode, 0x28, &((major << 8) | minor).to_le_bytes());
            } else {
                let dev = (minor & 0xFF) | (major << 8) | ((minor & !0xFF) << 12);
                put(&mut inode, 0x2C, &dev.to_le_bytes());
            }
        }
        // Nothing of a fifo's or a socket's is on disk but the inode.
        _ if matches!(node.mode & 0o170000, S_IFIFO | S_IFSOCK) => {}
        _ => {
            put(&mut inode, 0x20, &EXTENTS_FL.to_le_bytes());
            let extents = node.extents();
            if node.leaves.is_empty() {
                put(
                    &mut inode,
                    0x28,
                    &extent_header(extents.len(), INODE_EXTENTS, 0),
                );
                for (index, extent) in extents.iter().enumerate() {
                    put(&mut inode, 0x34 + 12 * index, extent);
                }
            } else {
                put(
                    &mut inode,
                    0x28,
                    &extent_header(node.leaves.len(), INODE_EXTENTS, 1),
                );
                for (index, (leaf, first)) in node
                    .leaves
                    .iter()
                    .zip(extents.chunks(LEAF_EXTENTS))
                    .enumerate()
                {
                    let mut entry = [0u8; 12];
                    entry[0..4].copy_from_slice(&first[0][0..4]); // the logical block it starts
                    entry[4..8].copy_from_slice(&(*leaf as u32).to_le_bytes());
                    entry[8..10].copy_from_slice(&((leaf >> 32) as u16).to_le_bytes());
                    put(&mut inode, 0x34 + 12 * index, &entry);
                }
            }
        }
    }

    if let Some((block, _)) = &node.xattrs {
        put(&mut inode, 0x68, &(*block as u32).to_le_bytes());
        put(&mut inode, 0x76, &((block >> 32) as u16).to_le_bytes());
    }
    put(&mut inode, 0x6C, &((node.size >> 32) as u32).to_le_bytes());
    put(&mut inode, 0x78, &((meta.uid >> 16) as u16).to_le_bytes());
    put(&mut inode, 0x7A, &((meta.gid >> 16) as u16).to_le_bytes());
    put(&mut inode, 0x80, &EXTRA_ISIZE.to_le_bytes());
    put(&mut inode, 0x84, &ctime_extra);
    put(&mut inode, 0x88, &mtime_extra);
    put(&mut inode, 0x8C, &atime_extra);
    put(&mut inode, 0x90, &crtime);
    put(&mut inode, 0x94, &crtime_extra);

    inode
}
