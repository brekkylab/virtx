//! A changeset's archive: the tar it travels to and from a registry in.

use std::collections::HashMap;
use std::io::{self, Read, Write};

use oci_spec::image::Sha256Digest;
use sha2::{Digest as _, Sha256};
use tar::EntryType;

use super::tree::{Data, Entry, Meta, Tree};
use super::{Digest, Error, Form, MediaType, Result};

/// The marker for a path this layer deletes from the layers below: `.wh.<name>`.
const WHITEOUT: &[u8] = b".wh.";

/// The marker for a directory nothing from the layers below shows through.
const OPAQUE: &[u8] = b".wh..wh..opq";

/// The prefix PAX headers carry an extended attribute's name under, as `star` and GNU tar
/// write them.
const XATTR: &str = "SCHILY.xattr.";

/// One layer of an image -- files added, replaced and deleted over the layers below it --
/// as an archive, held as the stream of it uncompressed.
pub struct Changeset<'a> {
    tar: Box<dyn Read + Send + 'a>,
}

impl<'a> Changeset<'a> {
    /// `blob`, an archive as a registry serves it, of type `media_type`.
    ///
    /// Not compressed, gzipped or zstd-compressed. A nondistributable or encrypted
    /// archive is refused: the one is fetched from somewhere other than a registry, the
    /// other needs a key, and neither is in an image anybody pulls here.
    pub fn new(media_type: &MediaType, blob: impl Read + Send + 'a) -> Result<Changeset<'a>> {
        let tar: Box<dyn Read + Send + 'a> = match media_type {
            MediaType::ImageLayer => Box::new(blob),
            MediaType::ImageLayerGzip => Box::new(flate2::read::GzDecoder::new(blob)),
            // Docker's name for the same thing.
            MediaType::Other(other)
                if other == "application/vnd.docker.image.rootfs.diff.tar.gzip" =>
            {
                Box::new(flate2::read::GzDecoder::new(blob))
            }
            MediaType::ImageLayerZstd => Box::new(
                ruzstd::decoding::StreamingDecoder::new(blob)
                    .map_err(|e| Error::malformed(Form::Archive, format!("not zstd: {e}")))?,
            ),
            _ => {
                return Err(Error::unsupported(
                    Form::Archive,
                    format!("{media_type} is not a form this reads"),
                ));
            }
        };
        Ok(Changeset { tar })
    }

    /// Read the archive, and answer with the tree it holds and its `diff_id`: the digest
    /// of the archive uncompressed, which a caller holding the image config compares with
    /// the one it signs.
    pub fn to_tree(self) -> Result<(Tree, Digest)> {
        let mut hashing = Hashing::new(self.tar);
        let tree = read(&mut hashing)?;
        // The end of an archive is two empty blocks and whatever padding its writer
        // rounded up to, which a reader stops short of -- and which the digest covers.
        io::copy(&mut hashing, &mut io::sink())
            .map_err(|e| Error::io("uncompressing an archive", e))?;

        Ok((tree, hashing.digest()))
    }

    /// Write `tree` onto `out` as an uncompressed archive, and answer with its `diff_id`.
    ///
    /// The root is not written: an archive names what is in a layer, and a layer's root
    /// is the one below it.
    pub fn from_tree(tree: &Tree, out: impl Write) -> Result<Digest> {
        let mut builder = tar::Builder::new(Hashing::new(out));
        builder.mode(tar::HeaderMode::Complete);

        // The first path each inode was written under, which the paths after it link to.
        let mut written: HashMap<_, Vec<u8>> = HashMap::new();

        for (path, inode, entry) in tree.walk() {
            if path.is_empty() {
                continue;
            }
            let write = |e| Error::io(format!("writing {}", show(&path)), e);

            match entry {
                Entry::File { meta, data } => match written.get(&inode) {
                    Some(first) => {
                        let mut header = header(EntryType::Link, meta, 0);
                        builder
                            .append_link(&mut header, utf8(&path)?, utf8(first)?)
                            .map_err(write)?;
                    }
                    None => {
                        let data = data.contents()?;
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
                    let mut header = header(EntryType::Directory, meta, 0);
                    builder
                        .append_data(&mut header, utf8(&path)?, io::empty())
                        .map_err(write)?;
                    if *opaque {
                        let marker = join(&path, OPAQUE);
                        let mut header = header_marker();
                        builder
                            .append_data(&mut header, utf8(&marker)?, io::empty())
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
                Entry::Whiteout => {
                    let (parent, name) = split(&path);
                    let mut marker = WHITEOUT.to_vec();
                    marker.extend_from_slice(name);
                    let marker = join(parent, &marker);
                    let mut header = header_marker();
                    builder
                        .append_data(&mut header, utf8(&marker)?, io::empty())
                        .map_err(write)?;
                }
            }
        }

        let hashing = builder
            .into_inner()
            .map_err(|e| Error::io("finishing an archive", e))?;
        Ok(hashing.digest())
    }
}

/// Read an uncompressed archive into a tree.
fn read(tar: impl Read) -> Result<Tree> {
    let mut tree = Tree::new();
    let mut archive = tar::Archive::new(tar);
    let entries = archive
        .entries()
        .map_err(|e| Error::malformed(Form::Archive, format!("not a tar archive: {e}")))?;

    for entry in entries {
        let mut entry =
            entry.map_err(|e| Error::malformed(Form::Archive, format!("reading an entry: {e}")))?;

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
            mtime: header.mtime().unwrap_or(0) as i64,
            ..Meta::default()
        };
        meta.xattrs = xattrs(&mut entry)?;

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
                entry
                    .read_to_end(&mut data)
                    .map_err(|e| Error::io(format!("reading {}", show(&path)), e))?;
                Entry::File {
                    meta,
                    data: Data::Buffer(data),
                }
            }
            EntryType::Symlink => Entry::Symlink {
                meta,
                target: link.ok_or_else(|| {
                    Error::malformed(
                        Form::Archive,
                        format!("{} is a symlink to nothing", show(&path)),
                    )
                })?,
            },
            // A second name for a file the archive already gave.
            EntryType::Link => {
                let target = link.ok_or_else(|| {
                    Error::malformed(
                        Form::Archive,
                        format!("{} is a hard link to nothing", show(&path)),
                    )
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

fn in_archive(e: Error) -> Error {
    e.reading(Form::Archive)
}

/// An entry's extended attributes, from its PAX header.
fn xattrs(
    entry: &mut tar::Entry<'_, impl Read>,
) -> Result<std::collections::BTreeMap<Vec<u8>, Vec<u8>>> {
    let mut xattrs = std::collections::BTreeMap::new();
    let extensions = entry
        .pax_extensions()
        .map_err(|e| Error::malformed(Form::Archive, format!("reading a pax header: {e}")))?;
    for extension in extensions.into_iter().flatten() {
        let extension = extension
            .map_err(|e| Error::malformed(Form::Archive, format!("reading a pax header: {e}")))?;
        if let Some(name) = extension.key_bytes().strip_prefix(XATTR.as_bytes()) {
            xattrs.insert(name.to_vec(), extension.value_bytes().to_vec());
        }
    }
    Ok(xattrs)
}

/// Write `meta`'s extended attributes as the PAX header of the entry at `path`, which
/// comes next.
fn pax(builder: &mut tar::Builder<impl Write>, meta: &Meta, path: &[u8]) -> Result<()> {
    if meta.xattrs.is_empty() {
        return Ok(());
    }
    let keys = meta
        .xattrs
        .keys()
        .map(|name| {
            std::str::from_utf8(name)
                .map(|name| format!("{XATTR}{name}"))
                .map_err(|_| {
                    Error::unwritable(
                        Form::Archive,
                        format!(
                            "{} has an xattr named {:?}, which is not UTF-8",
                            show(path),
                            String::from_utf8_lossy(name)
                        ),
                    )
                })
        })
        .collect::<Result<Vec<_>>>()?;
    builder
        .append_pax_extensions(
            keys.iter()
                .map(String::as_str)
                .zip(meta.xattrs.values().map(Vec::as_slice)),
        )
        .map_err(|e| Error::io(format!("writing {}", show(path)), e))
}

fn header(kind: EntryType, meta: &Meta, size: u64) -> tar::Header {
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(kind);
    header.set_mode(u32::from(meta.mode));
    header.set_uid(u64::from(meta.uid));
    header.set_gid(u64::from(meta.gid));
    header.set_mtime(meta.mtime.max(0) as u64);
    header.set_size(size);
    header
}

/// The header of a whiteout or opaque marker: an empty file that says something about
/// the path beside it.
fn header_marker() -> tar::Header {
    header(EntryType::Regular, &Meta::default(), 0)
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
        Error::unwritable(
            Form::Archive,
            format!("{} is not UTF-8, which a name has to be here", show(path)),
        )
    })
}

fn show(path: &[u8]) -> String {
    String::from_utf8_lossy(path).into_owned()
}

/// Bytes on their way through, hashed as they pass, so reading or writing an archive and
/// learning its `diff_id` are one pass and not two.
struct Hashing<T> {
    inner: T,
    hasher: Sha256,
}

impl<T> Hashing<T> {
    fn new(inner: T) -> Hashing<T> {
        Hashing {
            inner,
            hasher: Sha256::new(),
        }
    }

    fn digest(self) -> Digest {
        format!("{:x}", self.hasher.finalize())
            .parse::<Sha256Digest>()
            .expect("sha2 writes 64 lowercase hex characters")
            .into()
    }
}

impl<R: Read> Read for Hashing<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(buf)?;
        self.hasher.update(&buf[..read]);
        Ok(read)
    }
}

impl<W: Write> Write for Hashing<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let written = self.inner.write(buf)?;
        self.hasher.update(&buf[..written]);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}
