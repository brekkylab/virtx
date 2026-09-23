//! What a registry serves, and how to read it.
//!
//! A *layer blob* is a tar archive of one filesystem changeset, usually gzipped. This
//! module is the OCI end of an image: the archive's conventions, and the translation
//! out of them into the [`Tree`](crate::image::tree::Tree) every other side speaks.

use std::io::Read;

use crate::image::tree::{Directory, Extent, Meta, Node, Tree, Xattr, components};
use crate::image::{Error, Result};

/// The AUFS marker for a path this layer deletes.
const WHITEOUT: &[u8] = b".wh.";

/// The AUFS marker for a directory whose lower contents this layer hides entirely.
const OPAQUE: &[u8] = b".wh..wh..opq";

/// Read a layer archive into a [`Tree`].
///
/// Only the shape of the filesystem is kept. A file's contents stay in the archive
/// and are named by where they are in it, so the memory this costs follows how many
/// entries a layer has rather than how large it is.
///
/// Whiteouts are translated here, and this is the one place they can be. A layer
/// archive says "delete this" in AUFS's spelling, a file named `.wh.<name>`;
/// overlayfs in the guest wants a character device numbered 0:0, and an opaque
/// directory wants the `trusted.overlay.opaque` xattr. Neither can be made on a macOS
/// host, which is the other reason this reads the tar rather than unpacking it.
pub fn ingest(tar: &mut impl Read) -> Result<Tree> {
    use tar::EntryType;

    let mut tree = Tree::new();
    let mut archive = tar::Archive::new(tar);

    let entries = archive
        .entries()
        .map_err(|e| Error::Archive(format!("the blob is not a tar archive: {e}")))?;

    for entry in entries {
        let mut entry = entry.map_err(|e| Error::Archive(format!("reading an entry: {e}")))?;

        let kind = entry.header().entry_type();
        let path = match normalize(&entry.path_bytes())? {
            // The archive's own root. It has nothing to say that the tree does not
            // already hold.
            None => continue,
            Some(path) => path,
        };

        let header = entry.header();
        let mode = header.mode().unwrap_or(0o644) as u16 & 0o7777;
        let uid = header.uid().unwrap_or(0) as u32;
        let gid = header.gid().unwrap_or(0) as u32;
        let mtime = header.mtime().unwrap_or(0) as i64;
        let size = header.size().unwrap_or(0);
        let major = header.device_major().ok().flatten().unwrap_or(0);
        let minor = header.device_minor().ok().flatten().unwrap_or(0);
        let link = entry.link_name_bytes().map(|link| link.into_owned());
        let at = entry.raw_file_position();

        let mut meta = Meta {
            mode,
            uid,
            gid,
            mtime,
            xattrs: xattrs(&mut entry)?,
        };

        // A whiteout is a statement about the entry beside it, not an entry of its
        // own, so it is answered before anything is inserted.
        let (parent, name) = parent_and_name(&path);
        if name == OPAQUE {
            tree.make_dir(parent)
                .map_err(|_| traversal(&path))?
                .meta
                .set_xattr(b"trusted.overlay.opaque", b"y");
            continue;
        }
        if let Some(hidden) = name.strip_prefix(WHITEOUT) {
            let hidden = join(parent, hidden);
            tree.remove(&hidden);
            meta.mode = 0;
            tree.insert(
                &hidden,
                Node::Device {
                    meta,
                    block: false,
                    major: 0,
                    minor: 0,
                },
            )
            .map_err(|_| traversal(&path))?;
            continue;
        }

        let node = match kind {
            EntryType::Directory => Node::Directory(Directory::new(meta)),

            EntryType::Regular | EntryType::Continuous => Node::File {
                meta,
                id: tree.next_inode(),
                data: Extent {
                    offset: at,
                    len: size,
                },
            },

            EntryType::Symlink => Node::Symlink {
                meta,
                target: link.ok_or_else(|| {
                    Error::Archive(format!("{} is a symlink to nothing", show(&path)))
                })?,
            },

            // A hard link is a second name for an inode the archive already gave, so
            // it is resolved here rather than becoming a node of its own.
            EntryType::Link => {
                let target = link.ok_or_else(|| {
                    Error::Archive(format!("{} is a hard link to nothing", show(&path)))
                })?;
                let target = normalize(&target)?
                    .ok_or_else(|| Error::Archive(format!("{} links to the root", show(&path))))?;

                match tree.get(&target) {
                    Some(Node::File { id, data, .. }) => Node::File {
                        meta,
                        id: *id,
                        data: *data,
                    },
                    _ => {
                        return Err(Error::Archive(format!(
                            "{} is a hard link to {}, which the layer has not named",
                            show(&path),
                            show(&target)
                        )));
                    }
                }
            }

            EntryType::Char => Node::Device {
                meta,
                block: false,
                major,
                minor,
            },
            EntryType::Block => Node::Device {
                meta,
                block: true,
                major,
                minor,
            },
            EntryType::Fifo => Node::Fifo(meta),

            // Extended headers, and anything a later tar invents. The crate has
            // already folded the ones that carry names and xattrs into the entries
            // they belong to.
            _ => continue,
        };

        tree.insert(&path, node).map_err(|_| traversal(&path))?;
    }

    Ok(tree)
}

/// A path as the tree holds them: no leading `./`, no trailing slash, no empty
/// components. `None` is the archive's root. `..` is refused rather than resolved,
/// because a layer has no business naming anything above itself.
fn normalize(path: &[u8]) -> Result<Option<Vec<u8>>> {
    let mut out = Vec::with_capacity(path.len());

    for component in components(path) {
        if component == b"." {
            continue;
        }
        if component == b".." {
            return Err(traversal(path));
        }
        if !out.is_empty() {
            out.push(b'/');
        }
        out.extend_from_slice(component);
    }

    Ok((!out.is_empty()).then_some(out))
}

fn parent_and_name(path: &[u8]) -> (&[u8], &[u8]) {
    match path.iter().rposition(|byte| *byte == b'/') {
        Some(slash) => (&path[..slash], &path[slash + 1..]),
        None => (&[][..], path),
    }
}

fn join(parent: &[u8], name: &[u8]) -> Vec<u8> {
    if parent.is_empty() {
        return name.to_vec();
    }

    let mut path = Vec::with_capacity(parent.len() + 1 + name.len());
    path.extend_from_slice(parent);
    path.push(b'/');
    path.extend_from_slice(name);
    path
}

/// An entry's extended attributes, as `star` and GNU tar write them into a PAX
/// header. `trusted.overlay.*` arrives this way when a layer was built from an
/// already-converted one, which is why it is read and not only written.
fn xattrs(entry: &mut tar::Entry<'_, impl Read>) -> Result<Vec<Xattr>> {
    const PREFIX: &[u8] = b"SCHILY.xattr.";

    let extensions = entry
        .pax_extensions()
        .map_err(|e| Error::Archive(format!("reading a pax header: {e}")))?;

    let Some(extensions) = extensions else {
        return Ok(Vec::new());
    };

    let mut xattrs = Vec::new();
    for extension in extensions {
        let extension =
            extension.map_err(|e| Error::Archive(format!("reading a pax header: {e}")))?;
        if let Some(name) = extension.key_bytes().strip_prefix(PREFIX) {
            xattrs.push(Xattr {
                name: name.to_vec(),
                value: extension.value_bytes().to_vec(),
            });
        }
    }

    Ok(xattrs)
}

fn traversal(path: &[u8]) -> Error {
    Error::Archive(format!("{} leads out of the layer", show(path)))
}

fn show(path: &[u8]) -> String {
    String::from_utf8_lossy(path).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use tar::{EntryType, Header};

    /// Build a tar in memory. `(path, kind, link target, contents)`.
    fn archive(entries: &[(&str, EntryType, &str, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());

        for (path, kind, link, contents) in entries {
            let mut header = Header::new_gnu();
            header.set_entry_type(*kind);
            header.set_mode(0o644);
            header.set_uid(0);
            header.set_gid(0);
            header.set_mtime(0);
            header.set_size(contents.len() as u64);

            if !link.is_empty() {
                header.set_size(0);
                header.set_link_name(link).unwrap();
            }

            header.set_cksum();
            builder.append_data(&mut header, path, *contents).unwrap();
        }

        builder.into_inner().unwrap()
    }

    fn read(entries: &[(&str, EntryType, &str, &[u8])]) -> Tree {
        ingest(&mut Cursor::new(archive(entries))).expect("ingest")
    }

    #[test]
    fn a_plain_layer_becomes_the_tree_it_describes() {
        let tree = read(&[
            ("./", EntryType::Directory, "", b""),
            ("./etc/", EntryType::Directory, "", b""),
            ("./etc/hostname", EntryType::Regular, "", b"alpine\n"),
            ("./bin/sh", EntryType::Symlink, "/bin/busybox", b""),
        ]);

        assert!(tree.dir(b"etc").is_some());
        match tree.get(b"etc/hostname") {
            Some(Node::File { data, .. }) => assert_eq!(data.len, 7),
            other => panic!("{other:?}"),
        }
        match tree.get(b"bin/sh") {
            Some(Node::Symlink { target, .. }) => assert_eq!(target, b"/bin/busybox"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_whiteout_becomes_the_device_overlayfs_reads_as_a_deletion() {
        let tree = read(&[
            ("./etc/", EntryType::Directory, "", b""),
            ("./etc/.wh.resolv.conf", EntryType::Regular, "", b""),
        ]);

        match tree.get(b"etc/resolv.conf") {
            Some(Node::Device {
                block: false,
                major: 0,
                minor: 0,
                ..
            }) => {}
            other => panic!("{other:?}"),
        }
        assert!(
            tree.get(b"etc/.wh.resolv.conf").is_none(),
            "the marker itself is not an entry"
        );
    }

    #[test]
    fn a_whiteout_replaces_what_an_earlier_entry_in_the_same_layer_left() {
        let tree = read(&[
            ("./var/log", EntryType::Regular, "", b"noise"),
            ("./var/.wh.log", EntryType::Regular, "", b""),
        ]);

        assert!(matches!(tree.get(b"var/log"), Some(Node::Device { .. })));
    }

    #[test]
    fn an_opaque_marker_becomes_an_xattr_on_the_directory_holding_it() {
        let tree = read(&[
            ("./var/cache/", EntryType::Directory, "", b""),
            ("./var/cache/.wh..wh..opq", EntryType::Regular, "", b""),
            ("./var/cache/kept", EntryType::Regular, "", b"x"),
        ]);

        let dir = tree.dir(b"var/cache").expect("var/cache");
        assert_eq!(
            dir.meta
                .xattrs
                .iter()
                .find(|xattr| xattr.name == b"trusted.overlay.opaque")
                .map(|xattr| xattr.value.as_slice()),
            Some(&b"y"[..])
        );
        assert!(dir.entries.contains_key(&b"kept"[..]));
        assert!(!dir.entries.contains_key(&b".wh..wh..opq"[..]));
    }

    #[test]
    fn a_hard_link_reaches_the_inode_the_archive_already_named() {
        let tree = read(&[
            ("./bin/busybox", EntryType::Regular, "", b"ELF..."),
            ("./bin/ls", EntryType::Link, "bin/busybox", b""),
        ]);

        let (busybox, ls) = match (tree.get(b"bin/busybox"), tree.get(b"bin/ls")) {
            (
                Some(Node::File {
                    id: a, data: da, ..
                }),
                Some(Node::File {
                    id: b, data: db, ..
                }),
            ) => ((*a, *da), (*b, *db)),
            other => panic!("{other:?}"),
        };

        assert_eq!(busybox, ls, "one inode, one range of the archive");
        assert_eq!(tree.link_counts()[&busybox.0], 2);
    }

    #[test]
    fn a_hard_link_to_nothing_is_refused() {
        let error = ingest(&mut Cursor::new(archive(&[(
            "./bin/ls",
            EntryType::Link,
            "bin/busybox",
            b"",
        )])));

        assert!(matches!(error, Err(Error::Archive(_))), "{error:?}");
    }

    #[test]
    fn paths_are_reduced_to_what_the_tree_holds() {
        assert_eq!(
            normalize(b"./etc/hostname").unwrap(),
            Some(b"etc/hostname".to_vec())
        );
        assert_eq!(
            normalize(b"/etc//hostname/").unwrap(),
            Some(b"etc/hostname".to_vec())
        );
        assert_eq!(normalize(b"./").unwrap(), None, "the archive's own root");
        assert_eq!(normalize(b"").unwrap(), None);
    }

    /// The `tar` crate refuses to *write* such a path, so this goes at `normalize`
    /// rather than through an archive. An archive from elsewhere can still carry one.
    #[test]
    fn a_path_that_climbs_out_of_the_layer_is_refused() {
        for path in [&b"../escape"[..], b"etc/../../escape", b"etc/.."] {
            assert!(
                matches!(normalize(path), Err(Error::Archive(_))),
                "{} was accepted",
                String::from_utf8_lossy(path)
            );
        }
    }

    #[test]
    fn a_later_entry_wins_over_an_earlier_one() {
        let tree = read(&[
            ("./etc/hostname", EntryType::Regular, "", b"first"),
            ("./etc/hostname", EntryType::Regular, "", b"second-longer"),
        ]);

        match tree.get(b"etc/hostname") {
            Some(Node::File { data, .. }) => assert_eq!(data.len, 13),
            other => panic!("{other:?}"),
        }
    }
}
