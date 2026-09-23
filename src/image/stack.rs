//! Several layers, read as one filesystem.
//!
//! A layer is a changeset and not a filesystem: it says what it adds, what it
//! replaces, and -- in overlayfs's spelling, which is what [`blob`](crate::image::blob)
//! translates a tar archive's into -- what it deletes. Stacking applies those
//! statements bottom to top and leaves a tree that is only what the image *is*.
//!
//! The markers do not survive that. A character device numbered `0:0` means "whatever
//! is under this is not here", and once the layers under it have been read there is
//! nothing left for it to say; a directory carrying `trusted.overlay.opaque` means
//! "nothing from below is in here", and once below has been left out the xattr is
//! describing a filesystem nobody will stack again. Both are dropped, because what
//! comes out of here is a root a guest mounts and not a lower an overlay reads. A
//! lone layer goes through the same merge for that reason: a bottom layer carries no
//! whiteouts, but if one did, a device numbered `0:0` in the root of a booted image
//! is not what it meant.
//!
//! # Moving a layer onto the disk
//!
//! What comes back from [`read`](crate::image::erofs::read) is addressed within its
//! own layer image -- byte offsets into that file, and inode ids counted from zero in
//! it. A stack puts several such images on one disk, so both have to be moved before
//! they can be merged: [`place`] shifts a tree's extents by where its layer starts on
//! the disk, and its inode ids past every id already spoken for. Without the second,
//! two layers' files would collide in the merged tree's [`link_counts`], and the
//! writer would give one file's bytes to another file's name.
//!
//! [`link_counts`]: crate::image::tree::Tree::link_counts

use crate::image::tree::{Directory, Node, Tree};

/// The xattr a layer marks a directory with when it hides everything under it.
const OPAQUE: &[u8] = b"trusted.overlay.opaque";

/// Move `tree` onto the disk: its bytes to `data`, its inode ids past `ids`.
///
/// Returns the first id still free afterwards, which is what the next layer is placed
/// past. `data` is where the layer's image begins on the disk, in bytes.
pub fn place(tree: &mut Tree, data: u64, ids: u32) -> u32 {
    fn walk(dir: &mut Directory, data: u64, ids: u32, next: &mut u32) {
        for node in dir.entries.values_mut() {
            match node {
                Node::File { id, data: at, .. } => {
                    at.offset += data;
                    id.0 += ids;
                    *next = (*next).max(id.0 + 1);
                }
                Node::Directory(child) => walk(child, data, ids, next),
                _ => {}
            }
        }
    }

    let mut next = ids;
    walk(&mut tree.root, data, ids, &mut next);
    next
}

/// Merge `layers`, bottom first, into the filesystem they describe.
///
/// Later layers win: a file replaces what is under its name, a whiteout deletes it,
/// and an opaque directory empties what was below before anything of its own goes in.
/// Every one of those markers is gone from what comes back.
pub fn merge(layers: impl IntoIterator<Item = Tree>) -> Tree {
    let mut merged = Tree::new();
    for layer in layers {
        // The root's own metadata is the topmost layer's that states it, like any
        // other directory's.
        merged.root.meta = layer.root.meta.clone();
        overlay(&mut merged.root, layer.root);
    }
    merged
}

/// Apply one layer's directory over what is already there.
fn overlay(lower: &mut Directory, upper: Directory) {
    for (name, node) in upper.entries {
        match node {
            // A whiteout is a deletion and not a node. What it stood over goes, and it
            // does not take its place.
            Node::Device {
                block: false,
                major: 0,
                minor: 0,
                ..
            } => {
                lower.entries.remove(&name);
            }

            Node::Directory(mut upper) => {
                let opaque = strip_opaque(&mut upper);

                // A directory over a directory keeps what is under it, unless it said
                // not to. Anything else it lands on is replaced, because a layer that
                // puts a directory where a file was has said which of the two the
                // image ends up with.
                let mut merged = match lower.entries.remove(&name) {
                    Some(Node::Directory(below)) if !opaque => below,
                    _ => Directory::new(upper.meta.clone()),
                };
                merged.meta = upper.meta.clone();

                // Recursed into even when there was nothing below, because the layer's
                // own markers are dropped on the way through whatever else it holds.
                overlay(&mut merged, upper);
                lower.entries.insert(name, Node::Directory(merged));
            }

            node => {
                lower.entries.insert(name, node);
            }
        }
    }
}

/// Take the opaque marker off a directory, and say whether it was there.
fn strip_opaque(dir: &mut Directory) -> bool {
    let before = dir.meta.xattrs.len();
    dir.meta.xattrs.retain(|xattr| xattr.name != OPAQUE);
    dir.meta.xattrs.len() != before
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::tree::{Extent, InodeId, Meta, Node};

    fn meta() -> Meta {
        Meta {
            mode: 0o644,
            uid: 0,
            gid: 0,
            mtime: 0,
            xattrs: Vec::new(),
        }
    }

    fn file(id: u32, offset: u64) -> Node {
        Node::File {
            meta: meta(),
            id: InodeId(id),
            data: Extent { offset, len: 1 },
        }
    }

    fn whiteout() -> Node {
        Node::Device {
            meta: Meta { mode: 0, ..meta() },
            block: false,
            major: 0,
            minor: 0,
        }
    }

    fn base() -> Tree {
        let mut tree = Tree::new();
        tree.insert(b"etc/keep", file(0, 0)).unwrap();
        tree.insert(b"etc/replaced", file(1, 4096)).unwrap();
        tree.insert(b"etc/deleted", file(2, 8192)).unwrap();
        tree.insert(b"var/cache/stale", file(3, 12288)).unwrap();
        tree
    }

    fn upper() -> Tree {
        let mut tree = Tree::new();
        tree.insert(b"etc/replaced", file(0, 0)).unwrap();
        tree.insert(b"etc/added", file(1, 4096)).unwrap();
        tree.insert(b"etc/deleted", whiteout()).unwrap();

        let mut emptied = Directory::new(meta());
        emptied.meta.set_xattr(OPAQUE, b"y");
        tree.insert(b"var/cache", Node::Directory(emptied)).unwrap();
        tree
    }

    #[test]
    fn a_later_layer_wins_and_its_markers_do_not_survive() {
        let merged = merge([base(), upper()]);

        assert!(merged.get(b"etc/keep").is_some(), "the base's file went");
        assert!(
            merged.get(b"etc/added").is_some(),
            "the upper's did not come"
        );
        assert!(
            merged.get(b"etc/deleted").is_none(),
            "the whiteout did not delete"
        );
        assert!(
            merged.get(b"var/cache/stale").is_none(),
            "the opaque directory did not empty"
        );
        assert!(
            merged.dir(b"var/cache").is_some(),
            "and took itself with it"
        );
        assert!(
            merged
                .dir(b"var/cache")
                .unwrap()
                .meta
                .xattrs
                .iter()
                .all(|xattr| xattr.name != OPAQUE),
            "the opaque marker is still on a directory nothing will stack again"
        );

        // The upper's version of the replaced file, which its offset is enough to say.
        match merged.get(b"etc/replaced") {
            Some(Node::File { data, .. }) => assert_eq!(data.offset, 0),
            other => panic!("etc/replaced is {other:?}"),
        }
    }

    /// A lone layer goes through the merge too, which is the only thing that takes a
    /// marker off a bottom layer that carries one.
    #[test]
    fn one_layer_still_loses_its_markers() {
        let mut only = Tree::new();
        only.insert(b"etc/keep", file(0, 0)).unwrap();
        only.insert(b"etc/orphan", whiteout()).unwrap();

        let merged = merge([only]);
        assert!(merged.get(b"etc/keep").is_some());
        assert!(
            merged.get(b"etc/orphan").is_none(),
            "a whiteout over nothing is still not a file"
        );
    }

    #[test]
    fn placing_moves_the_bytes_and_the_ids_together() {
        let mut lower = base();
        let mut over = upper();

        let next = place(&mut lower, 8192, 0);
        assert_eq!(next, 4, "the base's four inodes are 0..4");
        let next = place(&mut over, 40960, next);
        assert_eq!(next, 6);

        match lower.get(b"etc/keep") {
            Some(Node::File { data, id, .. }) => {
                assert_eq!(data.offset, 8192);
                assert_eq!(*id, InodeId(0));
            }
            other => panic!("{other:?}"),
        }
        match over.get(b"etc/added") {
            Some(Node::File { data, id, .. }) => {
                assert_eq!(data.offset, 40960 + 4096);
                assert_eq!(*id, InodeId(5), "the upper's ids ran into the base's");
            }
            other => panic!("{other:?}"),
        }
    }

    /// The bug placing exists to prevent: two layers whose ids overlap merge into a
    /// tree that counts two files as one inode, and the writer then gives the second
    /// name the first file's bytes.
    #[test]
    fn placed_layers_keep_their_files_apart() {
        let mut lower = base();
        let mut over = upper();
        let next = place(&mut lower, 0, 0);
        place(&mut over, 40960, next);

        let merged = merge([lower, over]);
        let counts = merged.link_counts();
        assert!(
            counts.values().all(|links| *links == 1),
            "two layers' inodes were counted as one: {counts:?}"
        );
    }
}
