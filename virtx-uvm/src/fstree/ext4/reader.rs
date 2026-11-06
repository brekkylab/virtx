//! Reading an ext4 image as an [`FsTree`](crate::FsTree).
//!
//! What this reads is any ext4 a kernel or `mke2fs` makes with the features a Linux guest
//! mounts by default, and not only what [`FsTree::to_ext4`](crate::FsTree::to_ext4)
//! writes: the guest writes to the image, and nothing says it keeps to the writer's
//! choices. Names, symlink targets and xattr names are bytes, as Linux has them, and not
//! text.
//!
//! Checksums are not verified. An image read here was written by a guest that is down, and
//! one whose kernel found it inconsistent says so in the superblock.
//!
//! An image with a journal is read as its next mount would find it: whatever a guest killed
//! midway left committed in the journal is replayed over it, here and not in the image.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};

use crate::fstree::{Entry, Error, FsTree, Meta, Result, Timestamp};

/// How deep a directory may nest before this stops following it. A filesystem has no
/// directory inside itself, but an image that is not one can say it does.
const MAX_DEPTH: usize = 256;

/// The deepest an extent tree goes in a filesystem the kernel made.
const MAX_EXTENT_DEPTH: u16 = 5;

/// What overlayfs marks a directory with when it hides everything below it.
const OPAQUE: &[u8] = b"trusted.overlay.opaque";

pub(super) const ROOT: u32 = 2;

const S_IFMT: u16 = 0o170000;
const S_IFSOCK: u16 = 0o140000;
const S_IFLNK: u16 = 0o120000;
const S_IFREG: u16 = 0o100000;
const S_IFBLK: u16 = 0o060000;
const S_IFDIR: u16 = 0o040000;
const S_IFCHR: u16 = 0o020000;
const S_IFIFO: u16 = 0o010000;

const EXTENTS_FL: u32 = 0x8_0000;
const EXTENT_MAGIC: u16 = 0xF30A;
const XATTR_MAGIC: u32 = 0xEA02_0000;

/// jbd2's: the magic every journal block starts with, and what kind each is.
const JOURNAL_MAGIC: u32 = 0xC03B_3998;
const JOURNAL_DESCRIPTOR: u32 = 1;
const JOURNAL_COMMIT: u32 = 2;
const JOURNAL_SUPERBLOCK_V1: u32 = 3;
const JOURNAL_SUPERBLOCK_V2: u32 = 4;
const JOURNAL_REVOKE: u32 = 5;
/// The one journal feature this reads: revoke blocks, which every kernel journal has.
const JOURNAL_INCOMPAT_REVOKE: u32 = 0x1;
/// A tag's flags: the block's first four bytes were the magic and are zeros in the log,
/// the tag has no UUID of its own, the tag is its descriptor's last.
const TAG_ESCAPE: u16 = 0x1;
const TAG_SAME_UUID: u16 = 0x2;
const TAG_LAST: u16 = 0x8;

const COMPAT_HAS_JOURNAL: u32 = 0x4;
const INCOMPAT_FILETYPE: u32 = 0x2;
const INCOMPAT_RECOVER: u32 = 0x4;
const INCOMPAT_64BIT: u32 = 0x80;
/// What this reads, beyond what it has to know about: a journal to replay, extents, the
/// multiple-mount guard, groups whose metadata is together, a checksum seed, three-level
/// indexed directories, and case-insensitive lookup, none of which changes how a name or a
/// block is found.
const INCOMPAT_READ: u32 = INCOMPAT_FILETYPE
    | INCOMPAT_RECOVER
    | 0x40
    | INCOMPAT_64BIT
    | 0x100
    | 0x200
    | 0x2000
    | 0x4000
    | 0x2_0000;

/// An image, and where the things in it are.
pub(super) struct Reader<R> {
    image: R,
    block: u64,
    /// How long the image says it is, which nothing it points at may run past.
    bytes: u64,
    inodes_per_group: u32,
    inode_size: u64,
    /// The byte offset of the group descriptors, and how long each is.
    descriptors: u64,
    desc_size: u64,
    groups: u64,
    /// Whether a directory entry says what it names, or leaves that to the inode.
    filetype: bool,
    /// The blocks the journal's committed transactions left, by number, read in place of
    /// the image's.
    journaled: HashMap<u64, Vec<u8>>,
}

/// One inode, as far as this reads them.
pub(super) struct Inode {
    pub(super) mode: u16,
    size: u64,
    flags: u32,
    links: u16,
    /// Where it maps its blocks, or a short symlink's target, or a device's number.
    block: [u8; 60],
    meta: Meta,
}

impl<R: Read + Seek> Reader<R> {
    pub(super) fn open(mut image: R) -> Result<Reader<R>> {
        let mut sb = [0u8; 1024];
        image
            .seek(SeekFrom::Start(1024))
            .map_err(|e| Error::io("reading an ext4 image", e))?;
        image.read_exact(&mut sb).map_err(|_| {
            Error::Malformed("ext4 image: the file is too short to hold a superblock".into())
        })?;
        let u16_at = |at: usize| u16::from_le_bytes([sb[at], sb[at + 1]]);
        let u32_at = |at: usize| u32::from_le_bytes(sb[at..at + 4].try_into().unwrap());

        if u16_at(0x38) != 0xEF53 {
            return Err(Error::Malformed("ext4 image: not an ext4 image".into()));
        }
        let incompat = u32_at(0x60);
        let journal = u32_at(0x5C) & COMPAT_HAS_JOURNAL != 0;
        // Without a journal, cleared on mount and set again on a clean unmount: nothing
        // says what a guest that never unmounted left half done. With one, the kernel
        // leaves it set, and says so by needing recovery.
        if u16_at(0x3A) & 1 == 0 {
            return Err(Error::Malformed(
                "ext4 image: the filesystem was not left clean".into(),
            ));
        }
        if incompat & INCOMPAT_RECOVER != 0 && !journal {
            return Err(Error::Malformed(
                "ext4 image: the filesystem needs a journal replayed, and has none".into(),
            ));
        }
        if u16_at(0x3A) & 2 != 0 {
            return Err(Error::Malformed(
                "ext4 image: the kernel found the filesystem inconsistent".into(),
            ));
        }
        if incompat & !INCOMPAT_READ != 0 {
            return Err(Error::Unsupported(format!(
                "ext4 image: incompatible features {:#x}, which this does not read",
                incompat & !INCOMPAT_READ
            )));
        }

        let log = u32_at(0x18);
        if log > 6 {
            return Err(Error::Malformed(format!(
                "ext4 image: blocks of 2^{} bytes",
                u64::from(log) + 10
            )));
        }
        let block = 1024u64 << log;
        let mut blocks = u64::from(u32_at(0x04));
        if incompat & INCOMPAT_64BIT != 0 {
            blocks |= u64::from(u32_at(0x150)) << 32;
        }
        let first_data_block = u64::from(u32_at(0x14));
        let blocks_per_group = u64::from(u32_at(0x20));
        let inodes_per_group = u32_at(0x28);
        let inode_size = if u32_at(0x4C) == 0 {
            128
        } else {
            u64::from(u16_at(0x58))
        };
        let desc_size = if incompat & INCOMPAT_64BIT != 0 {
            u64::from(u16_at(0xFE))
        } else {
            32
        };
        if blocks_per_group == 0
            || inodes_per_group == 0
            || inode_size < 128
            || desc_size < 32
            || blocks <= first_data_block
        {
            return Err(Error::Malformed(
                "ext4 image: a superblock that describes no filesystem".into(),
            ));
        }

        let mut reader = Reader {
            image,
            block,
            bytes: blocks * block,
            inodes_per_group,
            inode_size,
            descriptors: (first_data_block + 1) * block,
            desc_size,
            groups: (blocks - first_data_block).div_ceil(blocks_per_group),
            filetype: incompat & INCOMPAT_FILETYPE != 0,
            journaled: HashMap::new(),
        };
        if incompat & INCOMPAT_RECOVER != 0 {
            let ino = u32_at(0xE0);
            if ino == 0 {
                return Err(Error::Unsupported(
                    "ext4 image: a journal on a device of its own".into(),
                ));
            }
            reader.journaled = reader.replay(ino)?;
        }
        Ok(reader)
    }

    fn at(&mut self, offset: u64, len: u64) -> Result<Vec<u8>> {
        // Asked before anything is allocated, so a size an image lies about cannot ask
        // for more memory than the image is long.
        if offset.saturating_add(len) > self.bytes {
            return Err(Error::Malformed(format!(
                "ext4 image: {len} bytes at {offset} run past the end of the image"
            )));
        }
        let mut bytes = vec![0u8; len as usize];
        self.image
            .seek(SeekFrom::Start(offset))
            .map_err(|e| Error::io("reading an ext4 image", e))?;
        self.image.read_exact(&mut bytes).map_err(|_| {
            Error::Malformed(format!(
                "ext4 image: the image ends before the {len} bytes it says are at {offset}"
            ))
        })?;
        if !self.journaled.is_empty() {
            for number in offset / self.block..(offset + len).div_ceil(self.block) {
                let Some(journaled) = self.journaled.get(&number) else {
                    continue;
                };
                let start = number * self.block;
                let (from, to) = (offset.max(start), (offset + len).min(start + self.block));
                bytes[(from - offset) as usize..(to - offset) as usize]
                    .copy_from_slice(&journaled[(from - start) as usize..(to - start) as usize]);
            }
        }
        Ok(bytes)
    }

    /// Inode `ino`, with its xattrs, from wherever they are.
    pub(super) fn inode(&mut self, ino: u32) -> Result<Inode> {
        let group = u64::from(ino.wrapping_sub(1) / self.inodes_per_group);
        if ino == 0 || group >= self.groups {
            return Err(Error::Malformed(format!(
                "ext4 image: inode {ino}, which the filesystem has no room for"
            )));
        }
        let desc = self.at(self.descriptors + group * self.desc_size, self.desc_size)?;
        let mut table = u64::from(u32::from_le_bytes(desc[0x08..0x0C].try_into().unwrap()));
        if self.desc_size >= 64 {
            table |= u64::from(u32::from_le_bytes(desc[0x28..0x2C].try_into().unwrap())) << 32;
        }
        let index = u64::from((ino - 1) % self.inodes_per_group);
        let raw = self.at(
            table * self.block + index * self.inode_size,
            self.inode_size,
        )?;
        let u16_at = |at: usize| u16::from_le_bytes([raw[at], raw[at + 1]]);
        let u32_at = |at: usize| u32::from_le_bytes(raw[at..at + 4].try_into().unwrap());

        // How much the inode holds past the original 128 bytes: the rest of its times, and
        // then the xattrs that fit beside it.
        let end = if self.inode_size > 128 {
            128 + usize::from(u16_at(0x80))
        } else {
            128
        };
        if end as u64 > self.inode_size {
            return Err(Error::Malformed(format!(
                "ext4 image: inode {ino} says it is longer than an inode is"
            )));
        }
        // The kernel's `ext4_decode_extra_time`: the seconds are signed, and two bits of
        // `_extra` are more epoch beneath the nanoseconds. An `_extra` the inode is too
        // short to have is none.
        let time = |at: usize, extra_at: usize| {
            let extra = if extra_at + 4 <= end {
                u32_at(extra_at)
            } else {
                0
            };
            Timestamp {
                secs: i64::from(u32_at(at) as i32) + (i64::from(extra & 3) << 32),
                nanos: extra >> 2,
            }
        };
        let crtime = if 0x94 <= end {
            time(0x90, 0x94)
        } else {
            Timestamp::ZERO
        };

        let mut xattrs = BTreeMap::new();
        let inline = &raw[end..];
        if inline.len() >= 4 && u32::from_le_bytes(inline[..4].try_into().unwrap()) == XATTR_MAGIC {
            read_xattrs(&inline[4..], &inline[4..], ino, &mut xattrs)?;
        }
        let acl = u64::from(u32_at(0x68)) | (u64::from(u16_at(0x76)) << 32);
        if acl != 0 {
            let block = self.at(acl * self.block, self.block)?;
            if u32::from_le_bytes(block[..4].try_into().unwrap()) != XATTR_MAGIC {
                return Err(Error::Malformed(format!(
                    "ext4 image: inode {ino}'s xattr block is not one"
                )));
            }
            read_xattrs(&block[32..], &block, ino, &mut xattrs)?;
        }

        let mode = u16_at(0x00);
        Ok(Inode {
            mode,
            size: u64::from(u32_at(0x04)) | (u64::from(u32_at(0x6C)) << 32),
            flags: u32_at(0x20),
            links: u16_at(0x1A),
            block: raw[0x28..0x64].try_into().unwrap(),
            meta: Meta {
                mode: mode & 0o7777,
                uid: u32::from(u16_at(0x02)) | (u32::from(u16_at(0x78)) << 16),
                gid: u32::from(u16_at(0x18)) | (u32::from(u16_at(0x7A)) << 16),
                atime: time(0x08, 0x8C),
                ctime: time(0x0C, 0x84),
                mtime: time(0x10, 0x88),
                crtime,
                xattrs,
            },
        })
    }

    /// An inode's contents, `size` bytes of them: what its extents map, and zeros where
    /// they map nothing or what they map is allocated but not yet written.
    fn contents(&mut self, ino: u32, inode: &Inode) -> Result<Vec<u8>> {
        if inode.flags & EXTENTS_FL == 0 {
            if inode.size == 0 {
                return Ok(Vec::new());
            }
            return Err(Error::Unsupported(format!(
                "ext4 image: inode {ino} maps its blocks without extents, as ext2 and ext3 did"
            )));
        }
        // Held whole, holes too, so a file has to fit in what the image could hold.
        if inode.size > self.bytes {
            return Err(Error::Unsupported(format!(
                "ext4 image: inode {ino} is {} bytes, more than the image could hold",
                inode.size
            )));
        }

        let mut extents = Vec::new();
        self.extents(ino, &inode.block, MAX_EXTENT_DEPTH, &mut extents)?;
        let mut data = vec![0u8; inode.size as usize];
        for (logical, first, count) in extents {
            let start = logical * self.block;
            if start >= inode.size {
                continue;
            }
            let len = (count * self.block).min(inode.size - start);
            let bytes = self.at(first * self.block, len)?;
            data[start as usize..(start + len) as usize].copy_from_slice(&bytes);
        }
        Ok(data)
    }

    /// The extents under `node`, as `(logical, first, count)`, written ones only: one that
    /// is allocated and not written reads as zeros, as a hole does.
    fn extents(
        &mut self,
        ino: u32,
        node: &[u8],
        depth_left: u16,
        out: &mut Vec<(u64, u64, u64)>,
    ) -> Result<()> {
        let wrong = |what: &str| Error::Malformed(format!("ext4 image: inode {ino}: {what}"));
        let u16_at = |at: usize| u16::from_le_bytes([node[at], node[at + 1]]);
        let u32_at = |at: usize| u32::from_le_bytes(node[at..at + 4].try_into().unwrap());

        if node.len() < 12 || u16_at(0) != EXTENT_MAGIC {
            return Err(wrong("an extent tree that is not one"));
        }
        let entries = usize::from(u16_at(2));
        let depth = u16_at(6);
        if 12 + 12 * entries > node.len() {
            return Err(wrong("more extents than their node holds"));
        }
        if depth > depth_left {
            return Err(wrong("an extent tree deeper than one can be"));
        }

        for entry in 0..entries {
            let at = 12 + 12 * entry;
            if depth == 0 {
                let len = u16_at(at + 4);
                // Past 32768, the length of an extent allocated and not yet written.
                if len > 32768 {
                    continue;
                }
                let first = u64::from(u32_at(at + 8)) | (u64::from(u16_at(at + 6)) << 32);
                out.push((u64::from(u32_at(at)), first, u64::from(len)));
            } else {
                let child = u64::from(u32_at(at + 4)) | (u64::from(u16_at(at + 8)) << 32);
                let block = self.at(child * self.block, self.block)?;
                if u16::from_le_bytes([block[6], block[7]]) != depth - 1 {
                    return Err(wrong("an extent node not as deep as its parent says"));
                }
                self.extents(ino, &block, depth - 1, out)?;
            }
        }
        Ok(())
    }

    /// The image's blocks as the journal in inode `ino` says its committed transactions left
    /// them, by number: what the kernel's next mount would write, and nothing it would not.
    ///
    /// jbd2's format, big-endian throughout. A transaction is a run of log blocks under one
    /// sequence number: descriptor blocks, each followed by the blocks it tags -- copies of
    /// the image's blocks as the transaction left them -- and revoke blocks, ended by a
    /// commit block. A transaction without its commit block never happened.
    ///
    /// Only what a guest writes over an image without `64bit`, `metadata_csum` or
    /// `fast_commit` is read: four-byte block numbers, no checksums, no fast commits.
    fn replay(&mut self, ino: u32) -> Result<HashMap<u64, Vec<u8>>> {
        let wrong = |what: String| Error::Malformed(format!("ext4 image: the journal: {what}"));
        let be16 = |b: &[u8], at: usize| u16::from_be_bytes([b[at], b[at + 1]]);
        let be32 = |b: &[u8], at: usize| u32::from_be_bytes(b[at..at + 4].try_into().unwrap());

        let inode = self.inode(ino)?;
        let mut extents = Vec::new();
        self.extents(ino, &inode.block, MAX_EXTENT_DEPTH, &mut extents)?;
        // Block `n` of the journal, from wherever in the image it is.
        let log = |reader: &mut Self, n: u64| {
            let (logical, first, _) = extents
                .iter()
                .find(|(logical, _, count)| (*logical..logical + count).contains(&n))
                .ok_or_else(|| wrong(format!("block {n} is mapped to nothing")))?;
            reader.at((first + n - logical) * reader.block, reader.block)
        };

        let sb = log(self, 0)?;
        let version = be32(&sb, 0x04);
        if be32(&sb, 0x00) != JOURNAL_MAGIC
            || !matches!(version, JOURNAL_SUPERBLOCK_V1 | JOURNAL_SUPERBLOCK_V2)
        {
            return Err(wrong("its first block is not a journal superblock".into()));
        }
        if u64::from(be32(&sb, 0x0C)) != self.block {
            return Err(wrong(format!(
                "blocks of {} bytes, in a filesystem of {}",
                be32(&sb, 0x0C),
                self.block
            )));
        }
        if be32(&sb, 0x20) != 0 {
            return Err(Error::Malformed(
                "ext4 image: the kernel aborted the journal".into(),
            ));
        }
        if version == JOURNAL_SUPERBLOCK_V2 && be32(&sb, 0x28) & !JOURNAL_INCOMPAT_REVOKE != 0 {
            return Err(Error::Unsupported(format!(
                "ext4 image: journal features {:#x}, which this does not read",
                be32(&sb, 0x28) & !JOURNAL_INCOMPAT_REVOKE
            )));
        }
        let (maxlen, first) = (u64::from(be32(&sb, 0x10)), u64::from(be32(&sb, 0x14)));
        let (mut sequence, start) = (be32(&sb, 0x18), u64::from(be32(&sb, 0x1C)));
        // Where the log starts, which is nowhere when it is empty.
        if start == 0 {
            return Ok(HashMap::new());
        }
        if first == 0 || !(first..maxlen).contains(&start) {
            return Err(wrong(format!(
                "a log from block {start}, in blocks {first} to {maxlen}"
            )));
        }

        // The committed transactions' writes, as `(transaction, image block, log block,
        // escaped)` in the order they were logged, and the last transaction to revoke each
        // revoked block.
        let mut committed = Vec::new();
        let mut revoked: HashMap<u64, u32> = HashMap::new();
        let (mut writes, mut revokes) = (Vec::new(), Vec::new());
        // The log wraps from the journal's last block to its first, and is never longer.
        let next = |n: u64| if n + 1 >= maxlen { first } else { n + 1 };
        let (mut n, mut walked) = (start, 0);
        loop {
            if walked >= maxlen {
                return Err(wrong("a log that never ends".into()));
            }
            let b = log(self, n)?;
            // Past the last transaction is what an earlier lap left, under an older
            // sequence number, or nothing.
            if be32(&b, 0) != JOURNAL_MAGIC || be32(&b, 8) != sequence {
                break;
            }
            match be32(&b, 4) {
                JOURNAL_DESCRIPTOR => {
                    // Each tag: the block number, a checksum, the flags, and a UUID unless
                    // it is the last tag's.
                    let mut at = 12;
                    while at + 8 <= b.len() {
                        let flags = be16(&b, at + 6);
                        n = next(n);
                        walked += 1;
                        writes.push((u64::from(be32(&b, at)), n, flags & TAG_ESCAPE != 0));
                        if flags & TAG_LAST != 0 {
                            break;
                        }
                        at += if flags & TAG_SAME_UUID == 0 { 24 } else { 8 };
                    }
                }
                JOURNAL_REVOKE => {
                    let used = be32(&b, 12) as usize;
                    if !(16..=b.len()).contains(&used) {
                        return Err(wrong(format!("a revoke block of {used} bytes")));
                    }
                    revokes.extend(
                        b[16..used]
                            .as_chunks::<4>()
                            .0
                            .iter()
                            .map(|r| u64::from(be32(r, 0))),
                    );
                }
                JOURNAL_COMMIT => {
                    committed.extend(
                        writes
                            .drain(..)
                            .map(|(target, at, escaped)| (sequence, target, at, escaped)),
                    );
                    revoked.extend(revokes.drain(..).map(|target| (target, sequence)));
                    sequence = sequence.wrapping_add(1);
                }
                _ => break,
            }
            n = next(n);
            walked += 1;
        }

        // Later transactions over earlier ones, and none over a revoke in its own
        // transaction or after it.
        let mut last: HashMap<u64, (u64, bool)> = HashMap::new();
        for (sequence, target, at, escaped) in committed {
            let revoked = revoked
                .get(&target)
                .is_some_and(|by| by.wrapping_sub(sequence) as i32 >= 0);
            if !revoked {
                last.insert(target, (at, escaped));
            }
        }
        let mut blocks = HashMap::with_capacity(last.len());
        for (target, (at, escaped)) in last {
            if (target + 1) * self.block > self.bytes {
                return Err(wrong(format!("a write to block {target}, past the image")));
            }
            let mut data = log(self, at)?;
            if escaped {
                data[..4].copy_from_slice(&JOURNAL_MAGIC.to_be_bytes());
            }
            blocks.insert(target, data);
        }
        Ok(blocks)
    }

    /// A directory's entries, as `(name, inode)`, `.` and `..` and unused slots left out.
    ///
    /// Read linearly, block by block, which reads an indexed directory too: its index
    /// blocks look like blocks of one unused entry, so that a reader that does not know the
    /// index reads past them.
    fn entries(&mut self, ino: u32, inode: &Inode) -> Result<Vec<(Vec<u8>, u32)>> {
        let data = self.contents(ino, inode)?;
        let wrong = |what: &str| Error::Malformed(format!("ext4 image: directory {ino}: {what}"));

        let mut entries = Vec::new();
        for block in data.chunks(self.block as usize) {
            let mut at = 0;
            while at + 8 <= block.len() {
                let child = u32::from_le_bytes(block[at..at + 4].try_into().unwrap());
                let rec_len = match u16::from_le_bytes([block[at + 4], block[at + 5]]) {
                    // The kernel's `ext4_rec_len_from_disk`, for blocks of 64 KiB.
                    0 | 65535 => block.len(),
                    len => usize::from(len),
                };
                let name_len = if self.filetype {
                    usize::from(block[at + 6])
                } else {
                    usize::from(u16::from_le_bytes([block[at + 6], block[at + 7]]))
                };
                if rec_len < 8 || rec_len % 4 != 0 || at + rec_len > block.len() {
                    return Err(wrong("an entry that runs past the block holding it"));
                }
                if 8 + name_len > rec_len {
                    return Err(wrong("a name longer than the entry holding it"));
                }
                let name = &block[at + 8..at + 8 + name_len];
                if child != 0 && name != b"." && name != b".." {
                    if name.is_empty() {
                        return Err(wrong("an entry with no name"));
                    }
                    entries.push((name.to_vec(), child));
                }
                at += rec_len;
            }
        }
        Ok(entries)
    }
}

/// The xattrs in a run of entries, each value at its offset from `values`. In an inode the
/// offsets count from the first entry; in a block of its own, from the block's start.
fn read_xattrs(
    entries: &[u8],
    values: &[u8],
    ino: u32,
    out: &mut BTreeMap<Vec<u8>, Vec<u8>>,
) -> Result<()> {
    let wrong = |what: &str| Error::Malformed(format!("ext4 image: inode {ino}'s xattrs: {what}"));

    let mut at = 0;
    // Four zero bytes where the next entry would start end them.
    while at + 4 <= entries.len()
        && u32::from_le_bytes(entries[at..at + 4].try_into().unwrap()) != 0
    {
        if at + 16 > entries.len() {
            return Err(wrong("an entry that runs past them"));
        }
        let name_len = usize::from(entries[at]);
        let index = entries[at + 1];
        let offset = usize::from(u16::from_le_bytes([entries[at + 2], entries[at + 3]]));
        let inum = u32::from_le_bytes(entries[at + 4..at + 8].try_into().unwrap());
        let size = u32::from_le_bytes(entries[at + 8..at + 12].try_into().unwrap()) as usize;
        if at + 16 + name_len > entries.len() {
            return Err(wrong("a name that runs past them"));
        }
        if inum != 0 {
            return Err(Error::Unsupported(format!(
                "ext4 image: inode {ino} keeps an xattr's value in an inode of its own"
            )));
        }
        if offset + size > values.len() {
            return Err(wrong("a value that runs past them"));
        }

        let prefix: &[u8] = match index {
            0 => b"",
            1 => b"user.",
            2 => b"system.posix_acl_access",
            3 => b"system.posix_acl_default",
            4 => b"trusted.",
            6 => b"security.",
            7 => b"system.",
            8 => b"system.richacl",
            _ => {
                return Err(Error::Unsupported(format!(
                    "ext4 image: inode {ino} has an xattr under prefix {index}, which this \
                     does not know"
                )));
            }
        };
        let mut name = prefix.to_vec();
        name.extend_from_slice(&entries[at + 16..at + 16 + name_len]);
        let value = &values[offset..offset + size];
        let value = match index {
            2 | 3 => acl_from_ext4(value).ok_or_else(|| wrong("an ACL that is not one"))?,
            _ => value.to_vec(),
        };
        out.insert(name, value);

        at += (16 + name_len).next_multiple_of(4);
    }
    Ok(())
}

/// A POSIX ACL as ext4 stores it -- version 1, and the entries that name nobody in four
/// bytes -- in the layout the kernel hands one out in and every other form carries: version
/// 2, eight bytes an entry. `None` for something that is not an ACL.
fn acl_from_ext4(value: &[u8]) -> Option<Vec<u8>> {
    let (version, mut entries) = value.split_first_chunk::<4>()?;
    if u32::from_le_bytes(*version) != 1 {
        return None;
    }
    let mut out = 2u32.to_le_bytes().to_vec();
    while !entries.is_empty() {
        let (head, rest) = entries.split_first_chunk::<4>()?;
        out.extend_from_slice(head);
        entries = match u16::from_le_bytes([head[0], head[1]]) {
            // The owner, the owning group, the mask, and everyone else: no id, which reads
            // as `ACL_UNDEFINED_ID`.
            0x01 | 0x04 | 0x10 | 0x20 => {
                out.extend_from_slice(&u32::MAX.to_le_bytes());
                rest
            }
            // A named user or group.
            0x02 | 0x08 => {
                let (id, rest) = rest.split_first_chunk::<4>()?;
                out.extend_from_slice(id);
                rest
            }
            _ => return None,
        };
    }
    Some(out)
}

/// The tree being read into, and what the walk has to remember on the way.
pub(super) struct Walk {
    pub(super) tree: FsTree,
    /// The first path each file with more than one name was read at, so that the names
    /// after it are links to it and not copies of it.
    pub(super) files: HashMap<u32, Vec<u8>>,
    /// The directories being read, to refuse one that is inside itself.
    pub(super) open: HashSet<u32>,
}

impl Walk {
    /// The directory `ino` at `path`, and everything under it.
    pub(super) fn directory<R: Read + Seek>(
        &mut self,
        reader: &mut Reader<R>,
        ino: u32,
        inode: Inode,
        path: Vec<u8>,
        depth: usize,
    ) -> Result<()> {
        if depth > MAX_DEPTH {
            return Err(Error::Malformed(format!(
                "ext4 image: directories nest more than {MAX_DEPTH} deep"
            )));
        }
        if !self.open.insert(ino) {
            return Err(Error::Malformed(format!(
                "ext4 image: directory {ino} is inside itself"
            )));
        }
        // A path the tree would not take is the image's fault, since the image named it.
        let in_image = |e: Error| e.reading(|what| Error::Malformed(format!("ext4 image: {what}")));

        let entries = reader.entries(ino, &inode)?;
        let mut meta = inode.meta;
        let opaque = meta
            .xattrs
            .remove(OPAQUE)
            .is_some_and(|value| value == b"y");
        self.tree
            .insert(&path, Entry::Directory { meta, opaque })
            .map_err(in_image)?;

        for (name, child) in entries {
            let mut path = path.clone();
            if !path.is_empty() {
                path.push(b'/');
            }
            path.extend_from_slice(&name);

            let inode = reader.inode(child)?;
            let entry = match inode.mode & S_IFMT {
                S_IFDIR => {
                    self.directory(reader, child, inode, path, depth + 1)?;
                    continue;
                }
                S_IFREG => {
                    if inode.links > 1 {
                        if let Some(first) = self.files.get(&child) {
                            self.tree.link(&path, first).map_err(in_image)?;
                            continue;
                        }
                        self.files.insert(child, path.clone());
                    }
                    Entry::File {
                        data: reader.contents(child, &inode)?,
                        meta: inode.meta,
                    }
                }
                S_IFLNK => Entry::Symlink {
                    // Kept where the inode would map its blocks when it is short enough,
                    // and then mapped by nothing.
                    target: if inode.size < 60 && inode.flags & EXTENTS_FL == 0 {
                        inode.block[..inode.size as usize].to_vec()
                    } else {
                        reader.contents(child, &inode)?
                    },
                    meta: inode.meta,
                },
                kind @ (S_IFCHR | S_IFBLK) => {
                    // The kernel's two encodings: the old one in the first word, the new
                    // one in the second where the first is empty.
                    let word =
                        |at: usize| u32::from_le_bytes(inode.block[at..at + 4].try_into().unwrap());
                    let (major, minor) = match word(0) {
                        0 => {
                            let dev = word(4);
                            (
                                (dev & 0xF_FF00) >> 8,
                                (dev & 0xFF) | ((dev >> 12) & 0xF_FF00),
                            )
                        }
                        dev => ((dev >> 8) & 0xFF, dev & 0xFF),
                    };
                    match (kind, major, minor) {
                        // What overlayfs deletes with.
                        (S_IFCHR, 0, 0) => Entry::Whiteout,
                        _ => Entry::Device {
                            meta: inode.meta,
                            block: kind == S_IFBLK,
                            major,
                            minor,
                        },
                    }
                }
                S_IFIFO => Entry::Fifo(inode.meta),
                S_IFSOCK => Entry::Socket(inode.meta),
                other => {
                    return Err(Error::Malformed(format!(
                        "ext4 image: {} is mode {other:o}, which is no kind of file",
                        String::from_utf8_lossy(&path)
                    )));
                }
            };
            self.tree.insert(&path, entry).map_err(in_image)?;
        }

        self.open.remove(&ino);
        Ok(())
    }
}
