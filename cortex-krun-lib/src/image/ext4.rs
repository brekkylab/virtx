//! Reading a session's writable layer back out, as a layer blob.
//!
//! This is [`blob`](crate::image::blob) run backwards. That module reads what a registry
//! serves and translates it into the [`Tree`](crate::image::tree::Tree) the rest of this
//! crate speaks; this one reads what a guest *wrote* and translates it into the archive a
//! registry would have served. The two meet in the middle: what comes out of here goes
//! straight into [`ingest`](crate::image::ingest), and the conventions cancel.
//!
//! # What is being read
//!
//! The upper layer of the overlay a session runs on -- see [`Ext4`](crate::session::Ext4)
//! for why it is a block device and not a directory. After a boot the guest has left a
//! filesystem on it holding exactly what the session changed: the files it wrote, and, for
//! each file it deleted, the marker overlayfs records a deletion as.
//!
//! The upperdir is a directory *on* that filesystem rather than its root -- libkrun's init
//! puts it at `upper/`, with `work/` beside it -- so what to export is named rather than
//! assumed.
//!
//! # The two translations
//!
//! Overlayfs and OCI spell the same two statements differently, and this is the one place
//! that knows both:
//!
//! | overlayfs, on the upper | OCI, in the archive |
//! |---|---|
//! | a character device numbered `0:0` | a file named `.wh.<name>` |
//! | `trusted.overlay.opaque` on a directory | a file named `.wh..wh..opq` inside it |
//!
//! Neither marker survives as itself. A whiteout device would be a device node in the
//! image, and the xattr would be a marker on a directory nothing will stack again.
//!
//! Overlayfs's *other* extended attributes -- `origin`, `impure`, `redirect`, `nlink`,
//! `metacopy` -- are dropped rather than translated. They are one kernel's bookkeeping
//! about one mount, and an image carrying them describes a stack that no longer exists.
//! Every other attribute, `security.capability` above all, travels.
//!
//! # What is not the session's work
//!
//! A mount point is made on the root the workload sees, which is the overlay -- so every
//! directory the boot created to hang a share on is recorded on the upper, and is this
//! backend's own rather than anything the session did. `skip` is how the caller names
//! them; see [`export`].

use std::collections::HashMap;
use std::io::Write;
use std::path::Path;

use ext4::{Checksums, Enhanced, Options, SuperBlock};

use crate::image::layer::Hashing;
use crate::image::{Digest, Error, Result};

/// The AUFS marker for a path a layer deletes. The other half of
/// [`blob`](crate::image::blob)'s constant of the same name.
const WHITEOUT: &str = ".wh.";

/// The AUFS marker for a directory whose lower contents a layer hides entirely.
const OPAQUE: &str = ".wh..wh..opq";

/// What overlayfs marks a directory with when it hides everything below it. The one
/// attribute of its own that means something to an image, and it is carried as a file
/// rather than as itself.
const OVERLAY_OPAQUE: &str = "trusted.overlay.opaque";

/// Everything overlayfs writes about its own mount. A layer has no use for any of it.
const OVERLAY: &str = "trusted.overlay.";

/// How an extended attribute is spelled in a PAX header, which is how `star` and GNU tar
/// write one and what [`ingest`](crate::image::ingest) reads back.
const SCHILY: &str = "SCHILY.xattr.";

/// Write what the session on `disk` wrote onto `out` as an uncompressed layer archive,
/// and answer with the archive's digest -- which is the layer's `diff_id`.
///
/// `disk` is the ext4 image the session's upper layer is on, and `from` is the directory
/// on it that overlayfs used as the upperdir -- `upper` for a boot this repository set up.
/// Both are named rather than assumed, because what makes them what they are is the boot's
/// arrangement and not anything this can read off the filesystem.
///
/// `skip` names guest paths this layer must not carry, as the guest spells them
/// (`/.cortex`, `/work`). What they are for is the boot's own scaffolding: a share is
/// mounted over a directory, making that directory is a write, and a write lands on the
/// upper -- so without this the layer would claim an empty `/.cortex` that nothing asked
/// for. A path that is not there costs nothing, so a caller can name every mount point it
/// might have made.
///
/// The digest comes back rather than being computed again afterwards, because the bytes go
/// past once either way -- the same reason
/// [`decompress_blob`](crate::image::decompress_blob) answers with one.
///
/// **The filesystem has to be quiescent.** What is read is what reached the file, and a
/// guest that was killed with writes still in its page cache has not put them there. The
/// caller is what knows the machine is down; see the `sync` before it.
pub fn to_blob(disk: &Path, from: &str, skip: &[&str], out: impl Write) -> Result<Digest> {
    let file = std::fs::File::open(disk)
        .map_err(|e| Error::io(format!("opening {}", disk.display()), e))?;
    // `Checksums::Enabled` and not the default `Required`: every checksum that is there
    // is verified, and a filesystem that carries none is still read. The one being read
    // here was formatted by [`arcbox_ext4`], which writes no `metadata_csum` -- so
    // requiring them would refuse every disk this is ever pointed at, over a feature the
    // filesystem was deliberately made without.
    let options = Options {
        checksums: Checksums::Enabled,
    };
    let fs = SuperBlock::new_with_options(file, &options)
        .map_err(|e| failed(disk, "reading", e))?;

    let at = fs
        .resolve_path(from)
        .map_err(|e| failed(disk, &format!("looking for {from} on"), e))?;
    let root = fs
        .load_inode(at.inode)
        .map_err(|e| failed(disk, &format!("reading {from} on"), e))?;

    // As they appear inside the layer, which is where they are compared. A caller names
    // them the way the guest does, absolutely, because that is the spelling it has.
    let skip: Vec<&str> = skip.iter().map(|path| path.trim_start_matches('/')).collect();

    let mut archive = tar::Builder::new(Hashing::new(out));
    // Every regular file with more than one name, against the first name it was given.
    // A second name is a hard link and is written as one, so that the layer holds one
    // file rather than two copies of it.
    let mut linked: HashMap<u32, String> = HashMap::new();

    walk(&fs, disk, &root, "", &skip, &mut linked, &mut archive)?;

    let hashed = archive
        .into_inner()
        .map_err(|e| Error::io("finishing the layer archive", e))?;
    Ok(hashed.digest())
}

/// Write one directory's entries, and recurse into the directories among them.
///
/// `at` is where this directory is inside the layer, `""` for the layer's own root.
fn walk<W: Write>(
    fs: &SuperBlock<std::fs::File>,
    disk: &Path,
    dir: &ext4::Inode,
    at: &str,
    skip: &[&str],
    linked: &mut HashMap<u32, String>,
    archive: &mut tar::Builder<W>,
) -> Result<()> {
    let listing = match fs.enhance(dir).map_err(|e| failed(disk, "reading", e))? {
        Enhanced::Directory(listing) => listing,
        other => {
            return Err(Error::Ext4(format!(
                "{at:?} on {} is {other:?}, and a layer is walked from a directory",
                disk.display()
            )));
        }
    };

    // By name, so that one filesystem makes one archive whatever order the directory
    // blocks happened to hold its entries in. A layer is named by the digest of these
    // bytes, and a digest that moved because a directory was rewritten would be a
    // different layer for the same contents.
    let mut listing = listing;
    listing.sort_by(|a, b| a.name.cmp(&b.name));

    for entry in listing {
        if entry.name == "." || entry.name == ".." {
            continue;
        }

        let path = if at.is_empty() {
            entry.name.clone()
        } else {
            format!("{at}/{}", entry.name)
        };
        if skip.contains(&path.as_str()) {
            continue;
        }

        let inode = fs
            .load_inode(entry.inode)
            .map_err(|e| failed(disk, &format!("reading {path} on"), e))?;
        let enhanced = fs
            .enhance(&inode)
            .map_err(|e| failed(disk, &format!("reading {path} on"), e))?;

        match enhanced {
            // A whiteout, which is a deletion and not a device. Written as the file the
            // archive says one with, beside the name it deletes rather than under it.
            Enhanced::CharacterDevice(0, 0) => {
                let (parent, name) = split(&path);
                let marker = join(parent, &format!("{WHITEOUT}{name}"));
                let mut header = tar::Header::new_gnu();
                header.set_entry_type(tar::EntryType::Regular);
                header.set_mode(0);
                header.set_size(0);
                header.set_mtime(u64::from(inode.stat.mtime.epoch_secs));
                archive
                    .append_data(&mut header, &marker, std::io::empty())
                    .map_err(|e| wrote(&marker, e))?;
            }

            Enhanced::Directory(_) => {
                let attrs = attributes(&inode);
                // The directory itself first, so that what goes inside it -- the opaque
                // marker, and then its entries -- lands under a name the archive has
                // already given.
                let named = format!("{path}/");
                let mut header = tar::Header::new_gnu();
                header.set_entry_type(tar::EntryType::Directory);
                meta(&mut header, &inode);
                header.set_size(0);
                pax(archive, &attrs.xattrs, &named)?;
                archive
                    .append_data(&mut header, &named, std::io::empty())
                    .map_err(|e| wrote(&named, e))?;

                if attrs.opaque {
                    let marker = join(&path, OPAQUE);
                    let mut header = tar::Header::new_gnu();
                    header.set_entry_type(tar::EntryType::Regular);
                    header.set_mode(0o644);
                    header.set_size(0);
                    header.set_mtime(u64::from(inode.stat.mtime.epoch_secs));
                    archive
                        .append_data(&mut header, &marker, std::io::empty())
                        .map_err(|e| wrote(&marker, e))?;
                }

                walk(fs, disk, &inode, &path, skip, linked, archive)?;
            }

            Enhanced::RegularFile => {
                // A second name for an inode already written is a hard link, and is
                // written as one: two copies would be two files in the image, and the
                // archive has a way of saying they are one.
                if inode.stat.link_count > 1
                    && let Some(first) = linked.get(&entry.inode)
                {
                    let mut header = tar::Header::new_gnu();
                    header.set_entry_type(tar::EntryType::Link);
                    meta(&mut header, &inode);
                    header.set_size(0);
                    archive
                        .append_link(&mut header, &path, first)
                        .map_err(|e| wrote(&path, e))?;
                    continue;
                }
                if inode.stat.link_count > 1 {
                    linked.insert(entry.inode, path.clone());
                }

                let attrs = attributes(&inode);
                let mut header = tar::Header::new_gnu();
                header.set_entry_type(tar::EntryType::Regular);
                meta(&mut header, &inode);
                header.set_size(inode.stat.size);
                pax(archive, &attrs.xattrs, &path)?;

                // Streamed rather than held: a build can write a file larger than this
                // process wants to keep, and the archive is being written to a file
                // anyway. `open` bounds the read to the inode's size, so what lands in
                // the entry is the file and not the blocks behind it.
                let contents = fs
                    .open(&inode)
                    .map_err(|e| failed(disk, &format!("reading {path} on"), e))?;
                archive
                    .append_data(&mut header, &path, contents)
                    .map_err(|e| wrote(&path, e))?;
            }

            Enhanced::SymbolicLink(target) => {
                let attrs = attributes(&inode);
                let mut header = tar::Header::new_gnu();
                header.set_entry_type(tar::EntryType::Symlink);
                meta(&mut header, &inode);
                header.set_size(0);
                pax(archive, &attrs.xattrs, &path)?;
                archive
                    .append_link(&mut header, &path, &target)
                    .map_err(|e| wrote(&path, e))?;
            }

            Enhanced::CharacterDevice(major, minor) | Enhanced::BlockDevice(major, minor) => {
                let block = matches!(enhanced, Enhanced::BlockDevice(..));
                let attrs = attributes(&inode);
                let mut header = tar::Header::new_gnu();
                header.set_entry_type(if block {
                    tar::EntryType::Block
                } else {
                    tar::EntryType::Char
                });
                meta(&mut header, &inode);
                header.set_size(0);
                header
                    .set_device_major(u32::from(major))
                    .map_err(|e| wrote(&path, e))?;
                header
                    .set_device_minor(minor)
                    .map_err(|e| wrote(&path, e))?;
                pax(archive, &attrs.xattrs, &path)?;
                archive
                    .append_data(&mut header, &path, std::io::empty())
                    .map_err(|e| wrote(&path, e))?;
            }

            Enhanced::Fifo => {
                let attrs = attributes(&inode);
                let mut header = tar::Header::new_gnu();
                header.set_entry_type(tar::EntryType::Fifo);
                meta(&mut header, &inode);
                header.set_size(0);
                pax(archive, &attrs.xattrs, &path)?;
                archive
                    .append_data(&mut header, &path, std::io::empty())
                    .map_err(|e| wrote(&path, e))?;
            }

            // A socket is a name a running process bound, and it means nothing once that
            // process is gone. There is no archive entry for one, and an image that
            // shipped one would be shipping a dead endpoint.
            Enhanced::Socket => continue,
        }
    }

    Ok(())
}

/// What an entry's extended attributes come to: the ones worth carrying, and whether the
/// one that is not an attribute at all was there.
struct Attributes {
    /// `trusted.overlay.opaque` was set, so this directory hides what is under it.
    opaque: bool,
    /// Everything else overlayfs did not write, sorted, as `(name, value)`.
    xattrs: Vec<(String, Vec<u8>)>,
}

/// Read an inode's extended attributes, keeping what an image has a use for.
///
/// Everything under `trusted.overlay.` goes. `opaque` is the one that means something
/// here, and it comes back as a flag because the archive says it with a file; the rest is
/// one mount's bookkeeping about a stack that will not exist again.
fn attributes(inode: &ext4::Inode) -> Attributes {
    let mut xattrs: Vec<(String, Vec<u8>)> = inode
        .stat
        .xattrs
        .iter()
        .filter(|(name, _)| !name.starts_with(OVERLAY))
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    // For the same reason the listing above is: what is written has to follow from what
    // is on the disk and not from how a hash map happened to order it.
    xattrs.sort();

    Attributes {
        opaque: inode.stat.xattrs.contains_key(OVERLAY_OPAQUE),
        xattrs,
    }
}

/// Put the ownership and permissions on a header. The size and the type are the caller's,
/// which vary by what is being written.
fn meta(header: &mut tar::Header, inode: &ext4::Inode) {
    header.set_mode(u32::from(inode.stat.file_mode) & 0o7777);
    header.set_uid(u64::from(inode.stat.uid));
    header.set_gid(u64::from(inode.stat.gid));
    header.set_mtime(u64::from(inode.stat.mtime.epoch_secs));
}

/// Write the extended-attribute header for the entry that comes next.
///
/// A PAX extended header describes the entry *after* it, so this has to be called
/// immediately before the entry it belongs to and nothing may come between.
fn pax<W: Write>(
    archive: &mut tar::Builder<W>,
    xattrs: &[(String, Vec<u8>)],
    path: &str,
) -> Result<()> {
    if xattrs.is_empty() {
        return Ok(());
    }

    let records: Vec<(String, &[u8])> = xattrs
        .iter()
        .map(|(name, value)| (format!("{SCHILY}{name}"), value.as_slice()))
        .collect();

    archive
        .append_pax_extensions(records.iter().map(|(key, value)| (key.as_str(), *value)))
        .map_err(|e| wrote(path, e))
}

/// A path's directory and its last component.
fn split(path: &str) -> (&str, &str) {
    match path.rfind('/') {
        Some(slash) => (&path[..slash], &path[slash + 1..]),
        None => ("", path),
    }
}

fn join(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_string()
    } else {
        format!("{parent}/{name}")
    }
}

/// Something the filesystem could not be asked.
///
/// The failure is taken by what can be done with it rather than by its type: the crate
/// hands back an `anyhow::Error`, and naming that here would put anyhow in this crate's
/// dependencies to spell one argument. `{:#}` so that the whole chain is said -- what went
/// wrong in an extent tree is several sentences deep and only the outermost is useless.
fn failed(disk: &Path, doing: &str, e: impl std::fmt::Display) -> Error {
    Error::Ext4(format!("{doing} {}: {e:#}", disk.display()))
}

/// Something the archive could not be told.
fn wrote(path: &str, e: std::io::Error) -> Error {
    Error::io(format!("writing {path} into a layer archive"), e)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::io::Cursor;
    use std::path::PathBuf;

    use arcbox_ext4::constants::file_mode;
    use arcbox_ext4::{Formatter, FormatOptions};

    use super::*;
    use crate::image::tree::{Node, Tree};

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Scratch {
            let path =
                std::env::temp_dir().join(format!("cortex-krun-upper-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Scratch(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn xattrs(pairs: &[(&str, &[u8])]) -> HashMap<String, Vec<u8>> {
        pairs
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_vec()))
            .collect()
    }

    /// An ext4 holding what a boot leaves on one: an `upper/` with the session's
    /// writes in it, and a `work/` beside it that is overlayfs's own.
    fn upper(at: &Path) -> PathBuf {
        let disk = at.join("session.ext4");
        let mut fs = Formatter::with_options(&disk, FormatOptions::new(16 << 20)).unwrap();

        let dir = |fs: &mut Formatter, path: &str, xattrs: Option<&HashMap<String, Vec<u8>>>| {
            fs.create(path, file_mode::S_IFDIR | 0o755, None, None, None, None, None, xattrs)
                .unwrap();
        };
        let file = |fs: &mut Formatter, path: &str, body: &str, xattrs: Option<&HashMap<String, Vec<u8>>>| {
            let mut data = Cursor::new(body.as_bytes().to_vec());
            fs.create(
                path,
                file_mode::S_IFREG | 0o644,
                None,
                None,
                Some(&mut data),
                None,
                None,
                xattrs,
            )
            .unwrap();
        };

        // overlayfs's own, beside the upperdir and not in it.
        dir(&mut fs, "/work", None);

        file(&mut fs, "/upper/etc/added", "added\n", None);

        // What a deletion is on an upper layer: a character device numbered 0:0.
        // Made as an empty file here and turned into one below -- the formatter writes
        // the three kinds an OCI layer has, and a whiteout device is not among them.
        file(&mut fs, "/upper/etc/removed", "", None);

        // A directory that hides everything under it, which overlayfs says with an
        // xattr and an archive says with a file.
        dir(
            &mut fs,
            "/upper/var/cache",
            Some(&xattrs(&[("trusted.overlay.opaque", b"y")])),
        );

        // One attribute an image wants, and one that is overlayfs talking to itself.
        file(
            &mut fs,
            "/upper/bin/ping",
            "#!/bin/sh\n",
            Some(&xattrs(&[
                ("security.capability", b"\x01\x00\x00\x02"),
                ("trusted.overlay.origin", b"bookkeeping"),
            ])),
        );

        fs.create(
            "/upper/etc/link",
            file_mode::S_IFLNK | 0o777,
            Some("/etc/added"),
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();

        // The share the boot hangs the agent on: a directory this backend made and
        // the session did not.
        dir(&mut fs, "/upper/.cortex", None);

        fs.close().unwrap();

        // The same correction the real formatter makes -- see `session::ext4::corrected`.
        // Without it this fixture is not the filesystem a session runs on.
        crate::session::ext4::corrected(&disk).unwrap();

        whiteout(&disk, "upper/etc/removed");
        disk
    }

    /// Turn the empty file at `path` into the character device numbered 0:0 that
    /// overlayfs records a deletion as.
    ///
    /// By hand, because no formatter here writes one: the crate that makes these images
    /// writes what an OCI layer holds, and on an upper layer a deletion is exactly the
    /// thing an OCI layer does not have. Only the file-type bits of the inode's mode
    /// change; a whiteout's data is its absence, so there is nothing else to write.
    fn whiteout(disk: &Path, path: &str) {
        use std::io::{Seek, SeekFrom, Write as _};

        let mut reader = arcbox_ext4::Reader::new(disk).unwrap();
        let node = reader
            .tree()
            .lookup(Path::new(path))
            .unwrap_or_else(|| panic!("{path} is not on the image"));
        let ino = reader.tree().node(node).inode;

        let sb = reader.superblock().clone();
        let block_size = 1024u64 << sb.log_block_size;
        let group = (ino - 1) / sb.inodes_per_group;
        let within = u64::from((ino - 1) % sb.inodes_per_group);
        let table = u64::from(reader.get_group_descriptor(group).unwrap().inode_table_lo);
        let at = table * block_size + within * u64::from(sb.inode_size);

        let mut file = std::fs::OpenOptions::new().write(true).open(disk).unwrap();
        file.seek(SeekFrom::Start(at)).unwrap();
        // `i_mode` is the first field, and the top four bits are the file type.
        file.write_all(&file_mode::S_IFCHR.to_le_bytes()).unwrap();
    }

    /// Export the upper and read the archive back as the tree it describes, which is
    /// the round trip every caller of this actually makes.
    fn exported(disk: &Path) -> (Tree, Digest) {
        let mut blob = Vec::new();
        let diff_id = to_blob(disk, "/upper", &["/.cortex"], &mut blob).unwrap();
        let tree = crate::image::ingest(&mut Cursor::new(&blob)).unwrap();
        (tree, diff_id)
    }

    /// The whole of it: what the guest wrote comes back as the layer it is, with each
    /// of the two markers translated and neither of them left as itself.
    #[test]
    fn an_upper_layer_comes_back_as_the_layer_it_describes() {
        let scratch = Scratch::new("roundtrip");
        let disk = upper(&scratch.0);
        let (tree, diff_id) = exported(&disk);

        assert_eq!(diff_id.algorithm(), "sha256");

        assert!(
            matches!(tree.get(b"etc/added"), Some(Node::File { .. })),
            "the session's own file did not come back"
        );

        // The whiteout: a character device on the upper, `.wh.removed` in the archive,
        // and a character device again once the archive is read.
        match tree.get(b"etc/removed") {
            Some(Node::Device { block: false, major: 0, minor: 0, .. }) => {}
            other => panic!("the deletion came back as {other:?}"),
        }

        // The opaque marker: an xattr on the upper, `.wh..wh..opq` in the archive.
        let cache = tree.dir(b"var/cache").expect("var/cache is not a directory");
        assert!(
            cache.meta.xattrs.iter().any(|x| x.name == b"trusted.overlay.opaque"),
            "the opaque directory did not say so"
        );

        // `work/` is beside the upperdir, not in it.
        assert!(tree.get(b"work").is_none(), "overlayfs's workdir is in the layer");

        // What the boot made rather than what the session wrote.
        assert!(tree.get(b".cortex").is_none(), "a skipped path is in the layer");
    }

    /// The attribute an image is meant to carry travels; the one overlayfs keeps about
    /// its own mount does not.
    #[test]
    fn overlayfs_bookkeeping_does_not_reach_the_layer() {
        let scratch = Scratch::new("xattrs");
        let disk = upper(&scratch.0);
        let (tree, _) = exported(&disk);

        let ping = tree.get(b"bin/ping").expect("bin/ping is not in the layer");
        let names: Vec<&[u8]> = ping.meta().xattrs.iter().map(|x| x.name.as_slice()).collect();

        assert!(
            names.contains(&&b"security.capability"[..]),
            "a file capability was dropped: {names:?}"
        );
        assert!(
            !names.iter().any(|name| name.starts_with(b"trusted.overlay.")),
            "overlayfs's own bookkeeping reached the image: {names:?}"
        );
    }

    #[test]
    fn a_symlink_keeps_what_it_points_at() {
        let scratch = Scratch::new("symlink");
        let disk = upper(&scratch.0);
        let (tree, _) = exported(&disk);

        match tree.get(b"etc/link") {
            Some(Node::Symlink { target, .. }) => assert_eq!(target, b"/etc/added"),
            other => panic!("etc/link is {other:?}"),
        }
    }

    /// A layer is named by the digest of its archive, so one filesystem has to make
    /// one archive -- whatever order its directory blocks happened to be in.
    #[test]
    fn one_upper_makes_one_digest() {
        let scratch = Scratch::new("stable");
        let disk = upper(&scratch.0);

        let (_, first) = exported(&disk);
        let (_, again) = exported(&disk);
        assert_eq!(first, again);
    }
}
