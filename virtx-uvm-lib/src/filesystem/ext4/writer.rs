//! Writing a [`Tree`] as a new ext4 image.
//!
//! [`arcbox_ext4`]'s formatter does the writing: it is a library and not a program, which
//! is the only form that works on hosts with no `mke2fs`, and it writes the file sparse.
//! Two things it does not do are done here afterwards, on the image it closed:
//!
//! - **Devices and pipes.** It writes directories, regular files and symlinks, and
//!   refuses anything else -- and a whiteout is a device, so every deletion a layer
//!   carries is one of these. Each is written as the empty file it can write and then
//!   turned into what it is ([`special`]).
//! - **Two fields it gets wrong** about the filesystem it made ([`corrected`]).

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use arcbox_ext4::{FileTimestamps, Formatter};

use crate::filesystem::tree::{Entry, Meta, Tree};
use crate::filesystem::{Error, Form, Result};

/// Block size. 4096 is what every kernel mounts without argument.
const BLOCK_SIZE: u32 = 4096;

/// The type bits of `i_mode`.
const S_IFREG: u16 = 0o100000;
const S_IFDIR: u16 = 0o040000;
const S_IFLNK: u16 = 0o120000;
const S_IFCHR: u16 = 0o020000;
const S_IFBLK: u16 = 0o060000;
const S_IFIFO: u16 = 0o010000;

/// What overlayfs marks a directory with when it hides everything below it.
const OPAQUE: &str = "trusted.overlay.opaque";

/// Write `tree` as a new ext4 image at `path`, `size` bytes at most.
pub fn write(tree: &Tree, path: &Path, size: u64) -> Result<()> {
    let mut fs = Formatter::new(path, BLOCK_SIZE, size).map_err(|e| failed("formatting", e))?;

    // The first path each inode was written under, so that a second one is a link and not
    // a copy; and the nodes the formatter would not take, for `special` to finish.
    let mut written: HashMap<_, String> = HashMap::new();
    let mut deferred: Vec<Special> = Vec::new();

    for (at, inode, entry) in tree.walk() {
        if at.is_empty() {
            continue;
        }
        // A path the formatter takes is a `&str`, and a name in a tree is bytes. Refused
        // rather than made printable: a lossy name is a file under a name nobody asked for.
        let name = std::str::from_utf8(&at).map_err(|_| {
            Error::unwritable(
                Form::Ext4,
                format!(
                    "{:?} is not a name this can write onto an ext4 image",
                    String::from_utf8_lossy(&at)
                ),
            )
        })?;
        let on = format!("/{name}");
        let wrote = |e| failed(&format!("writing {on} onto"), e);

        match entry {
            Entry::Directory { meta, opaque } => {
                let mut xattrs = xattrs(meta)?.unwrap_or_default();
                if *opaque {
                    xattrs.insert(OPAQUE.into(), b"y".to_vec());
                }
                let xattrs = (!xattrs.is_empty()).then_some(xattrs);
                create(&mut fs, &on, S_IFDIR | meta.mode, None, None, meta, xattrs)
                    .map_err(wrote)?;
            }

            Entry::File { meta, data } => {
                if let Some(first) = written.get(&inode) {
                    fs.link(&on, first).map_err(wrote)?;
                    continue;
                }
                let data = data.contents()?;
                let mut bytes = &data[..];
                create(
                    &mut fs,
                    &on,
                    S_IFREG | meta.mode,
                    None,
                    Some(&mut bytes),
                    meta,
                    xattrs(meta)?,
                )
                .map_err(wrote)?;
                written.insert(inode, on.clone());
            }

            Entry::Symlink { meta, target } => {
                let target = std::str::from_utf8(target).map_err(|_| {
                    Error::unwritable(
                        Form::Ext4,
                        format!(
                            "{on} points at {:?}, which is not a target this can write",
                            String::from_utf8_lossy(target)
                        ),
                    )
                })?;
                create(
                    &mut fs,
                    &on,
                    S_IFLNK | meta.mode,
                    Some(target),
                    None,
                    meta,
                    xattrs(meta)?,
                )
                .map_err(wrote)?;
            }

            // The ones the formatter will not write: made as the empty file it can, which
            // is what one of these is but for its mode and the bytes that hold a device
            // number, and named for `special` to finish once the image is closed.
            Entry::Device { .. } | Entry::Fifo(_) | Entry::Whiteout => {
                let (mode, major, minor, meta) = match entry {
                    Entry::Device {
                        meta,
                        block,
                        major,
                        minor,
                    } => {
                        let kind = if *block { S_IFBLK } else { S_IFCHR };
                        (kind | meta.mode, *major, *minor, meta)
                    }
                    Entry::Fifo(meta) => (S_IFIFO | meta.mode, 0, 0, meta),
                    // What overlayfs deletes with: a character device numbered 0:0.
                    _ => (S_IFCHR, 0, 0, &Meta::default()),
                };
                create(&mut fs, &on, S_IFREG, None, None, meta, xattrs(meta)?).map_err(wrote)?;
                deferred.push(Special {
                    path: name.to_string(),
                    mode,
                    major,
                    minor,
                });
            }
        }
    }

    fs.close().map_err(|e| failed("formatting", e))?;

    // In this order and not the other: the first walks every inode on the image, and the
    // second only ever touches ones it has already been over.
    corrected(path)?;
    special(path, &deferred)
}

/// One inode, with what the tree said about it.
fn create(
    fs: &mut Formatter,
    at: &str,
    mode: u16,
    link: Option<&str>,
    data: Option<&mut dyn Read>,
    meta: &Meta,
    xattrs: Option<HashMap<String, Vec<u8>>>,
) -> std::result::Result<(), arcbox_ext4::error::FormatError> {
    fs.create(
        at,
        mode,
        link,
        Some(stamps(meta.mtime)),
        data,
        Some(meta.uid),
        Some(meta.gid),
        xattrs.as_ref(),
    )
}

/// An entry's extended attributes, in the shape the formatter takes them.
///
/// `None` rather than an empty map for an entry with none, because the formatter writes a
/// sentinel attribute beside whatever it is given, and an empty map would put one on every
/// inode on the image.
fn xattrs(meta: &Meta) -> Result<Option<HashMap<String, Vec<u8>>>> {
    if meta.xattrs.is_empty() {
        return Ok(None);
    }
    let mut named = HashMap::with_capacity(meta.xattrs.len());
    for (name, value) in &meta.xattrs {
        let name = std::str::from_utf8(name).map_err(|_| {
            Error::unwritable(
                Form::Ext4,
                format!(
                    "{:?} is not the name of an extended attribute",
                    String::from_utf8_lossy(name)
                ),
            )
        })?;
        named.insert(name.to_string(), value.clone());
    }
    Ok(Some(named))
}

/// When an entry was last written, as the four timestamps an inode has.
///
/// All four and not the one, because the other three would otherwise be read off this
/// host's clock -- and then the same tree would make a different image every time. A time
/// before the epoch is written as the epoch rather than as a number that reads as the far
/// future.
fn stamps(mtime: i64) -> FileTimestamps {
    let seconds = u32::try_from(mtime).unwrap_or(0);
    FileTimestamps {
        access_lo: seconds,
        access_hi: 0,
        modification_lo: seconds,
        modification_hi: 0,
        creation_lo: seconds,
        creation_hi: 0,
        now_lo: seconds,
        now_hi: 0,
    }
}

fn failed(doing: &str, e: impl std::fmt::Display) -> Error {
    Error::unwritable(Form::Ext4, format!("{doing} the image: {e}"))
}

//--------------------------------------------------------------------------------------------------
// Fixing up what the formatter wrote
//--------------------------------------------------------------------------------------------------

/// The inode every ext4 filesystem's root directory is.
const ROOT_INODE: u32 = 2;

/// Where the superblock records which operating system made the filesystem, and the value
/// that means Linux.
const CREATOR_OS: u64 = 0x400 + 0x48;
const LINUX: u32 = 0;

/// Where an inode keeps its flags, and the two acted on here.
const INODE_FLAGS: u64 = 0x20;
const HUGE_FILE: u32 = 0x4_0000;
const EXTENTS: u32 = 0x8_0000;

/// Where an inode's mode is, which is its first field.
const INODE_MODE: u64 = 0x00;

/// Where the sixty bytes an inode maps its blocks with are -- and, for a node that has no
/// blocks, where the number of the device it is goes instead.
const INODE_BLOCK: u64 = 0x28;
const INODE_BLOCK_LEN: usize = 60;

/// A node the formatter has no way to write, as what it is and where it went.
struct Special {
    /// Where it is, spelled the way the formatter's own tree spells one: no leading
    /// separator.
    path: String,
    /// The whole of `i_mode`, type bits included.
    mode: u16,
    /// The device it is; both zero for a pipe, and for the device that means a deletion.
    major: u32,
    minor: u32,
}

/// Where inodes are in the image at `disk`, as byte offsets: the ones `which` picks out
/// of the formatter's own reading of it.
fn locate(
    disk: &Path,
    which: impl FnOnce(&arcbox_ext4::Reader) -> Result<Vec<u32>>,
) -> Result<Vec<u64>> {
    let unreadable = |e: arcbox_ext4::error::ReadError| failed("reading", e);
    let mut reader = arcbox_ext4::Reader::new(disk).map_err(unreadable)?;
    let inodes = which(&reader)?;
    let sb = reader.superblock().clone();
    let block_size = 1024u64 << sb.log_block_size;

    let mut at = Vec::with_capacity(inodes.len());
    for ino in inodes {
        let group = (ino - 1) / sb.inodes_per_group;
        let within = u64::from((ino - 1) % sb.inodes_per_group);
        let table = u64::from(
            reader
                .get_group_descriptor(group)
                .map_err(unreadable)?
                .inode_table_lo,
        );
        at.push(table * block_size + within * u64::from(sb.inode_size));
    }
    Ok(at)
}

/// Correct the two things the formatter writes that describe this filesystem as something
/// it is not. Neither stops a guest mounting it; both matter because the image is read
/// again on the host, and a reader is right to believe what the metadata says.
///
/// - `s_creator_os` is written as FreeBSD. The field says how the twelve bytes of `osd2`
///   in every inode are laid out, and the high halves of an owner's uid and gid are in
///   there; these inodes are Linux's.
/// - `EXT4_HUGE_FILE_FL` is set on every inode it makes. It means "`i_blocks` counts
///   filesystem blocks and not sectors", which is a statement about files too large to
///   count in sectors, and none of these is one.
fn corrected(disk: &Path) -> Result<()> {
    // Every inode there is, which is the root and whatever the formatter put under it.
    let at = locate(disk, |reader| {
        let tree = reader.tree();
        let mut inodes = vec![ROOT_INODE];
        let mut walking = vec![tree.root()];
        while let Some(index) = walking.pop() {
            let node = tree.node(index);
            inodes.push(node.inode);
            walking.extend(node.children.iter().copied());
        }
        Ok(inodes)
    })?;

    let mut file = open(disk)?;
    file.seek(SeekFrom::Start(CREATOR_OS))
        .and_then(|_| file.write_all(&LINUX.to_le_bytes()))
        .map_err(|e| Error::io(format!("writing the superblock of {}", disk.display()), e))?;
    for offset in at {
        let flags = read_u32(&mut file, disk, offset + INODE_FLAGS)?;
        write_at(
            &mut file,
            disk,
            offset + INODE_FLAGS,
            &(flags & !HUGE_FILE).to_le_bytes(),
        )?;
    }
    file.sync_all()
        .map_err(|e| Error::io(format!("writing an inode of {}", disk.display()), e))
}

/// Turn each of `nodes` from the empty file it was written as into the device or pipe it
/// is: the type bits of its mode, the flag that says its blocks are mapped by an extent
/// tree -- which a node with no blocks must not carry -- and its device number, which
/// lives in the sixty bytes an ordinary file would map its blocks with.
fn special(disk: &Path, nodes: &[Special]) -> Result<()> {
    if nodes.is_empty() {
        return Ok(());
    }
    // Every one of them found before anything is written, because the reader holds the
    // file open and what follows writes to it.
    let at = locate(disk, |reader| {
        let tree = reader.tree();
        nodes
            .iter()
            .map(|node| {
                let found = tree.lookup(Path::new(&node.path)).ok_or_else(|| {
                    Error::unwritable(
                        Form::Ext4,
                        format!(
                            "{} is not on the image, having just been written",
                            node.path
                        ),
                    )
                })?;
                Ok(tree.node(found).inode)
            })
            .collect()
    })?;

    let mut file = open(disk)?;
    for (node, offset) in nodes.iter().zip(at) {
        write_at(
            &mut file,
            disk,
            offset + INODE_MODE,
            &node.mode.to_le_bytes(),
        )?;
        let flags = read_u32(&mut file, disk, offset + INODE_FLAGS)?;
        write_at(
            &mut file,
            disk,
            offset + INODE_FLAGS,
            &(flags & !EXTENTS).to_le_bytes(),
        )?;

        let (old, new) = device(node.major, node.minor);
        let mut block = [0u8; INODE_BLOCK_LEN];
        block[..4].copy_from_slice(&old.to_le_bytes());
        block[4..8].copy_from_slice(&new.to_le_bytes());
        write_at(&mut file, disk, offset + INODE_BLOCK, &block)?;
    }
    file.sync_all()
        .map_err(|e| Error::io(format!("writing an inode of {}", disk.display()), e))
}

/// A device number, in the two spellings an inode has for one: the old one in the first of
/// the sixty bytes for a device small enough to say that way -- a whiteout is 0:0 -- and the
/// wider one in the second for anything larger.
fn device(major: u32, minor: u32) -> (u32, u32) {
    if major < 256 && minor < 256 {
        ((major << 8) | minor, 0)
    } else {
        (0, (minor & 0xff) | (major << 8) | ((minor & !0xff) << 12))
    }
}

fn open(disk: &Path) -> Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(disk)
        .map_err(|e| Error::io(format!("opening {}", disk.display()), e))
}

fn read_u32(file: &mut std::fs::File, disk: &Path, offset: u64) -> Result<u32> {
    let mut bytes = [0u8; 4];
    file.seek(SeekFrom::Start(offset))
        .and_then(|_| file.read_exact(&mut bytes))
        .map_err(|e| Error::io(format!("reading an inode of {}", disk.display()), e))?;
    Ok(u32::from_le_bytes(bytes))
}

fn write_at(file: &mut std::fs::File, disk: &Path, offset: u64, bytes: &[u8]) -> Result<()> {
    file.seek(SeekFrom::Start(offset))
        .and_then(|_| file.write_all(bytes))
        .map_err(|e| Error::io(format!("writing an inode of {}", disk.display()), e))
}
