//! A changeset's archive: the tar one layer of an image travels to and from a registry in.

use std::collections::HashMap;
use std::io::{self, Read, Write};

use oci_spec::image::MediaType;
use tar::EntryType;

use super::tree::{Entry, FsTree, Meta, Timestamp};
use super::{Error, Result};

/// The marker for a path this layer deletes from the layers below: `.wh.<name>`.
const WHITEOUT: &[u8] = b".wh.";

/// The marker for a directory nothing from the layers below shows through.
const OPAQUE: &[u8] = b".wh..wh..opq";

/// The prefix PAX headers carry an extended attribute's name under, as `star` and GNU tar
/// write them.
const XATTR: &str = "SCHILY.xattr.";

/// The PAX record a file's creation time is in, as libarchive writes it. POSIX has none.
const CREATED: &[u8] = b"LIBARCHIVE.creationtime";

impl FsTree {
    /// The tree in `blob`, an archive as a registry serves it, of type `media_type`.
    ///
    /// Not compressed, gzipped or zstd-compressed. A nondistributable or encrypted
    /// archive is refused: the one is fetched from somewhere other than a registry, the
    /// other needs a key, and neither is in an image anybody pulls here.
    pub fn from_changeset(media_type: &MediaType, blob: impl Read) -> Result<FsTree> {
        match media_type {
            MediaType::ImageLayer => read(blob),
            MediaType::ImageLayerGzip => read(flate2::read::GzDecoder::new(blob)),
            // Docker's name for the same thing.
            MediaType::Other(other)
                if other == "application/vnd.docker.image.rootfs.diff.tar.gzip" =>
            {
                read(flate2::read::GzDecoder::new(blob))
            }
            MediaType::ImageLayerZstd => read(
                ruzstd::decoding::StreamingDecoder::new(blob)
                    .map_err(|e| Error::Malformed(format!("archive: not zstd: {e}")))?,
            ),
            _ => Err(Error::Unsupported(format!(
                "archive: {media_type} is not a form this reads"
            ))),
        }
    }

    /// Write the tree onto `blob` as an archive of type `media_type`, as a registry would
    /// serve it.
    ///
    /// Not compressed or gzipped. zstd is refused: the encoder this has takes what it
    /// compresses as something to read rather than write to, so the whole archive would
    /// have to be held first.
    pub fn to_changeset(&self, media_type: &MediaType, blob: impl Write) -> Result<()> {
        match media_type {
            MediaType::ImageLayer => write(self, blob),
            MediaType::ImageLayerGzip => {
                let mut gzip = flate2::write::GzEncoder::new(blob, flate2::Compression::default());
                write(self, &mut gzip)?;
                gzip.finish()
                    .map_err(|e| Error::io("compressing an archive", e))?;
                Ok(())
            }
            _ => Err(Error::Unsupported(format!(
                "archive: {media_type} is not a form this writes"
            ))),
        }
    }
}

/// Read an uncompressed archive into a tree.
fn read(tar: impl Read) -> Result<FsTree> {
    // A path the tree would not take is the archive's fault, since the archive named it.
    let in_archive = |e: Error| e.reading(|what| Error::Malformed(format!("archive: {what}")));

    let mut tree = FsTree::new();
    let mut archive = tar::Archive::new(tar);
    let entries = archive
        .entries()
        .map_err(|e| Error::Malformed(format!("archive: not a tar archive: {e}")))?;

    for entry in entries {
        let mut entry =
            entry.map_err(|e| Error::Malformed(format!("archive: reading an entry: {e}")))?;

        let path = entry.path_bytes().into_owned();
        let link = entry.link_name_bytes().map(|link| link.into_owned());
        let header = entry.header();
        let kind = header.entry_type();
        let major = header.device_major().ok().flatten().unwrap_or(0);
        let minor = header.device_minor().ok().flatten().unwrap_or(0);
        let mut meta = Meta {
            mode: header.mode().unwrap_or(0o644) as u16 & 0o7777,
            uid: header.uid().unwrap_or(0) as u32,
            gid: header.gid().unwrap_or(0) as u32,
            mtime: Timestamp {
                secs: header.mtime().unwrap_or(0) as i64,
                nanos: 0,
            },
            ..Meta::default()
        };
        let extensions = entry
            .pax_extensions()
            .map_err(|e| Error::Malformed(format!("archive: reading a pax header: {e}")))?;
        for extension in extensions.into_iter().flatten() {
            let extension = extension
                .map_err(|e| Error::Malformed(format!("archive: reading a pax header: {e}")))?;
            let (key, value) = (extension.key_bytes(), extension.value_bytes());
            if let Some(name) = key.strip_prefix(XATTR.as_bytes()) {
                meta.xattrs.insert(name.to_vec(), value.to_vec());
                continue;
            }
            let time = match key {
                b"mtime" => &mut meta.mtime,
                b"atime" => &mut meta.atime,
                b"ctime" => &mut meta.ctime,
                CREATED => &mut meta.crtime,
                _ => continue,
            };
            *time = pax_time(value).ok_or_else(|| {
                Error::Malformed(format!(
                    "archive: {} has a {} of {:?}, which is not a time",
                    String::from_utf8_lossy(&path),
                    String::from_utf8_lossy(key),
                    String::from_utf8_lossy(value)
                ))
            })?;
        }

        // A marker is a statement about the path beside it, not an entry of its own.
        let (parent, name) = split(&path);
        if name == OPAQUE {
            tree.opaque(parent).map_err(in_archive)?;
            continue;
        }
        if let Some(hidden) = name.strip_prefix(WHITEOUT) {
            tree.insert(&join(parent, hidden), Entry::Whiteout)
                .map_err(in_archive)?;
            continue;
        }

        let node = match kind {
            EntryType::Directory => Entry::Directory {
                meta,
                opaque: false,
            },
            EntryType::Regular | EntryType::Continuous => {
                let mut data = Vec::new();
                entry.read_to_end(&mut data).map_err(|e| {
                    Error::io(format!("reading {}", String::from_utf8_lossy(&path)), e)
                })?;
                Entry::File { meta, data }
            }
            EntryType::Symlink => Entry::Symlink {
                meta,
                target: link.ok_or_else(|| {
                    Error::Malformed(format!(
                        "archive: {} is a symlink to nothing",
                        String::from_utf8_lossy(&path)
                    ))
                })?,
            },
            // A second name for a file the archive already gave.
            EntryType::Link => {
                let target = link.ok_or_else(|| {
                    Error::Malformed(format!(
                        "archive: {} is a hard link to nothing",
                        String::from_utf8_lossy(&path)
                    ))
                })?;
                tree.link(&path, &target).map_err(in_archive)?;
                continue;
            }
            EntryType::Char => Entry::Device {
                meta,
                block: false,
                major,
                minor,
            },
            EntryType::Block => Entry::Device {
                meta,
                block: true,
                major,
                minor,
            },
            EntryType::Fifo => Entry::Fifo(meta),
            // Extended headers, and anything a later tar invents. The crate has already
            // folded the ones that carry names and xattrs into the entries they belong to.
            _ => continue,
        };
        tree.insert(&path, node).map_err(in_archive)?;
    }

    Ok(tree)
}

/// Write `tree` onto `tar` as an uncompressed archive.
///
/// The root is not written: an archive names what is in a layer, and a layer's root is the
/// one below it.
fn write(tree: &FsTree, tar: impl Write) -> Result<()> {
    let mut builder = tar::Builder::new(tar);
    builder.mode(tar::HeaderMode::Complete);

    // The first path each inode was written under, which the paths after it link to.
    let mut written: HashMap<_, Vec<u8>> = HashMap::new();

    for (path, inode, entry) in tree.walk() {
        if path.is_empty() {
            continue;
        }
        let write = |e| Error::io(format!("writing {}", String::from_utf8_lossy(&path)), e);

        match entry {
            Entry::File { meta, data } => match written.get(&inode) {
                Some(first) => {
                    let mut header = header(EntryType::Link, meta, 0);
                    builder
                        .append_link(&mut header, utf8(&path)?, utf8(first)?)
                        .map_err(write)?;
                }
                None => {
                    pax(&mut builder, meta, &path)?;
                    let mut header = header(EntryType::Regular, meta, data.len() as u64);
                    builder
                        .append_data(&mut header, utf8(&path)?, &data[..])
                        .map_err(write)?;
                    written.insert(inode, path.clone());
                }
            },
            Entry::Directory { meta, opaque } => {
                pax(&mut builder, meta, &path)?;
                let mut dir = header(EntryType::Directory, meta, 0);
                builder
                    .append_data(&mut dir, utf8(&path)?, io::empty())
                    .map_err(write)?;
                if *opaque {
                    let marker = join(&path, OPAQUE);
                    let mut marked = header(EntryType::Regular, &Meta::default(), 0);
                    builder
                        .append_data(&mut marked, utf8(&marker)?, io::empty())
                        .map_err(write)?;
                }
            }
            Entry::Symlink { meta, target } => {
                pax(&mut builder, meta, &path)?;
                let mut header = header(EntryType::Symlink, meta, 0);
                builder
                    .append_link(&mut header, utf8(&path)?, utf8(target)?)
                    .map_err(write)?;
            }
            Entry::Device {
                meta,
                block,
                major,
                minor,
            } => {
                pax(&mut builder, meta, &path)?;
                let kind = if *block {
                    EntryType::Block
                } else {
                    EntryType::Char
                };
                let mut header = header(kind, meta, 0);
                header.set_device_major(*major).map_err(write)?;
                header.set_device_minor(*minor).map_err(write)?;
                builder
                    .append_data(&mut header, utf8(&path)?, io::empty())
                    .map_err(write)?;
            }
            Entry::Fifo(meta) => {
                pax(&mut builder, meta, &path)?;
                let mut header = header(EntryType::Fifo, meta, 0);
                builder
                    .append_data(&mut header, utf8(&path)?, io::empty())
                    .map_err(write)?;
            }
            // A tar has no entry for one, and GNU tar leaves them out as this does.
            Entry::Socket(_) => {}
            Entry::Whiteout => {
                let (parent, name) = split(&path);
                let mut marker = WHITEOUT.to_vec();
                marker.extend_from_slice(name);
                let marker = join(parent, &marker);
                let mut header = header(EntryType::Regular, &Meta::default(), 0);
                builder
                    .append_data(&mut header, utf8(&marker)?, io::empty())
                    .map_err(write)?;
            }
        }
    }

    builder
        .into_inner()
        .map_err(|e| Error::io("finishing an archive", e))?;
    Ok(())
}

/// Write `meta`'s extended attributes as the PAX header of the entry at `path`, which
/// comes next.
fn pax(builder: &mut tar::Builder<impl Write>, meta: &Meta, path: &[u8]) -> Result<()> {
    let mut records: Vec<(String, Vec<u8>)> = Vec::new();
    for (name, value) in &meta.xattrs {
        let name = std::str::from_utf8(name).map_err(|_| {
            Error::Unwritable(format!(
                "archive: {} has an xattr named {:?}, which is not UTF-8",
                String::from_utf8_lossy(path),
                String::from_utf8_lossy(name)
            ))
        })?;
        records.push((format!("{XATTR}{name}"), value.clone()));
    }

    // The header holds whole seconds since the epoch and no other time; the rest is said
    // here, and only where there is something to say.
    let mut times = vec![
        ("atime", meta.atime),
        ("ctime", meta.ctime),
        ("LIBARCHIVE.creationtime", meta.crtime),
    ];
    times.retain(|(_, time)| *time != Timestamp::ZERO);
    if meta.mtime.nanos != 0 || meta.mtime.secs < 0 {
        times.push(("mtime", meta.mtime));
    }
    for (key, time) in times {
        // Decimal seconds, and a fraction for what is past the second. A negative time is
        // counted back from the epoch whole, fraction and all: -1.5 is a second and a half
        // before it.
        let (sign, secs, nanos) = match (time.secs < 0, time.nanos) {
            (false, nanos) => ("", time.secs.unsigned_abs(), nanos),
            (true, 0) => ("-", time.secs.unsigned_abs(), 0),
            (true, nanos) => ("-", (time.secs + 1).unsigned_abs(), 1_000_000_000 - nanos),
        };
        let value = match nanos {
            0 => format!("{sign}{secs}"),
            _ => format!("{sign}{secs}.{nanos:09}"),
        };
        records.push((key.to_string(), value.into_bytes()));
    }

    if records.is_empty() {
        return Ok(());
    }
    builder
        .append_pax_extensions(
            records
                .iter()
                .map(|(key, value)| (key.as_str(), value.as_slice())),
        )
        .map_err(|e| Error::io(format!("writing {}", String::from_utf8_lossy(path)), e))
}

/// A PAX time: decimal seconds since the epoch, a `-` before it for one before the epoch,
/// and a fraction past the second if there is one. Digits past the nanosecond are dropped.
fn pax_time(value: &[u8]) -> Option<Timestamp> {
    let value = std::str::from_utf8(value).ok()?;
    let (negative, value) = match value.strip_prefix('-') {
        Some(value) => (true, value),
        None => (false, value),
    };
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    if whole.is_empty()
        || !whole
            .bytes()
            .chain(fraction.bytes())
            .all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let secs: i64 = whole.parse().ok()?;
    let nanos: u32 = format!("{:0<9}", &fraction[..fraction.len().min(9)])
        .parse()
        .ok()?;
    Some(match (negative, nanos) {
        (false, _) => Timestamp { secs, nanos },
        (true, 0) => Timestamp { secs: -secs, nanos },
        (true, _) => Timestamp {
            secs: -secs - 1,
            nanos: 1_000_000_000 - nanos,
        },
    })
}

fn header(kind: EntryType, meta: &Meta, size: u64) -> tar::Header {
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(kind);
    header.set_mode(u32::from(meta.mode));
    header.set_uid(u64::from(meta.uid));
    header.set_gid(u64::from(meta.gid));
    header.set_mtime(meta.mtime.secs.max(0) as u64);
    header.set_size(size);
    header
}

/// A path as the directory holding it and its own name.
fn split(path: &[u8]) -> (&[u8], &[u8]) {
    let end = path
        .iter()
        .rposition(|byte| *byte != b'/')
        .map_or(0, |i| i + 1);
    let path = &path[..end];
    match path.iter().rposition(|byte| *byte == b'/') {
        Some(slash) => (&path[..slash], &path[slash + 1..]),
        None => (&[][..], path),
    }
}

fn join(parent: &[u8], name: &[u8]) -> Vec<u8> {
    let mut path = parent.to_vec();
    if !path.is_empty() {
        path.push(b'/');
    }
    path.extend_from_slice(name);
    path
}

/// A path as the `tar` crate takes one. It writes names out of `Path`s, so one that is
/// not UTF-8 cannot be written on every host this builds for.
fn utf8(path: &[u8]) -> Result<&str> {
    std::str::from_utf8(path).map_err(|_| {
        Error::Unwritable(format!(
            "archive: {} is not UTF-8, which a name has to be here",
            String::from_utf8_lossy(path)
        ))
    })
}
