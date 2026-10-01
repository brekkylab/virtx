//! The shape of a filesystem, held in memory.
//!
//! What every side of an image conversion speaks. [`crate::image::blob`] reads one out
//! of a layer archive, [`crate::image::erofs`] writes one down as an image, and a merge
//! of several layers is one again -- so this belongs to neither of them.
//!
//! A file's contents are not here. [`Node::File`] carries an [`Extent`] instead, which
//! says where in some archive the bytes are, so what a tree costs follows how many
//! entries it has rather than how large they are.

//! A layer's filesystem, held between the tar that described it and the EROFS that
//! will carry it.
//!
//! This exists because the two ends disagree about order. A tar is a sequence: an
//! entry is whatever the entry before it left behind, and a later entry may delete
//! or replace an earlier one. An EROFS is a placement: an inode's name is its offset
//! in the metadata area, a directory entry names its children by that offset, and a
//! hard link is one inode counted from several directories. Nothing can be written
//! until every entry is known, so everything is read first.
//!
//! What is *not* here is file contents. A node points at the bytes in the tar it came
//! from ([`Extent`]), and the writer reads them again. A layer therefore costs memory
//! in proportion to how many entries it has, not how large they are.

use std::collections::BTreeMap;

/// The type bits of `i_mode`, as every Unix has spelled them. Not EROFS's own dirent
/// numbering, which is narrower and lives with the format.
pub const S_IFMT: u16 = 0o170000;
pub const S_IFSOCK: u16 = 0o140000;
pub const S_IFLNK: u16 = 0o120000;
pub const S_IFREG: u16 = 0o100000;
pub const S_IFBLK: u16 = 0o060000;
pub const S_IFDIR: u16 = 0o040000;
pub const S_IFCHR: u16 = 0o020000;
pub const S_IFIFO: u16 = 0o010000;

/// Where a regular file's bytes are in the uncompressed tar the tree was read from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Extent {
    pub offset: u64,
    pub len: u64,
}

/// Names the inode behind a path. Hard links are several paths holding one of these,
/// which is what makes them one inode in the image and not a copy each.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct InodeId(pub u32);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Xattr {
    pub name: Vec<u8>,
    pub value: Vec<u8>,
}

/// What every node has, whatever kind it is. `mode` is permission and setuid bits
/// only; the type bits come from the [`Node`] variant, so the two cannot disagree.
#[derive(Clone, Debug)]
pub struct Meta {
    pub mode: u16,
    pub uid: u32,
    pub gid: u32,
    pub mtime: i64,
    pub xattrs: Vec<Xattr>,
}

impl Meta {
    /// What an implied parent directory gets — one a tar never named, because tars
    /// are allowed to leave them out.
    pub fn implied_dir() -> Meta {
        Meta {
            mode: 0o755,
            uid: 0,
            gid: 0,
            mtime: 0,
            xattrs: Vec::new(),
        }
    }

    /// Set an xattr, replacing one of the same name.
    pub fn set_xattr(&mut self, name: &[u8], value: &[u8]) {
        match self.xattrs.iter_mut().find(|xattr| xattr.name == name) {
            Some(existing) => existing.value = value.to_vec(),
            None => self.xattrs.push(Xattr {
                name: name.to_vec(),
                value: value.to_vec(),
            }),
        }
    }
}

#[derive(Clone, Debug)]
pub enum Node {
    File {
        meta: Meta,
        /// Shared by every path hard-linked to this one.
        id: InodeId,
        data: Extent,
    },
    Directory(Directory),
    Symlink {
        meta: Meta,
        target: Vec<u8>,
    },
    Device {
        meta: Meta,
        block: bool,
        major: u32,
        minor: u32,
    },
    Fifo(Meta),
}

impl Node {
    pub fn meta(&self) -> &Meta {
        match self {
            Node::File { meta, .. }
            | Node::Symlink { meta, .. }
            | Node::Device { meta, .. }
            | Node::Fifo(meta) => meta,
            Node::Directory(dir) => &dir.meta,
        }
    }

    /// The full `i_mode`, type bits included.
    pub fn mode(&self) -> u16 {
        let kind = match self {
            Node::File { .. } => S_IFREG,
            Node::Directory(_) => S_IFDIR,
            Node::Symlink { .. } => S_IFLNK,
            Node::Device { block: true, .. } => S_IFBLK,
            Node::Device { block: false, .. } => S_IFCHR,
            Node::Fifo(_) => S_IFIFO,
        };

        kind | (self.meta().mode & 0o7777)
    }

    pub fn is_dir(&self) -> bool {
        matches!(self, Node::Directory(_))
    }
}

#[derive(Clone, Debug)]
pub struct Directory {
    pub meta: Meta,
    /// Sorted by name, which is the order EROFS wants dirents in. Keeping the tree
    /// sorted rather than sorting at write time is what makes the image a function
    /// of the tree alone, and so the same bytes every run.
    pub entries: BTreeMap<Vec<u8>, Node>,
}

impl Directory {
    pub fn new(meta: Meta) -> Directory {
        Directory {
            meta,
            entries: BTreeMap::new(),
        }
    }
}

/// One layer's filesystem.
#[derive(Clone, Debug)]
pub struct Tree {
    pub root: Directory,
    next_id: u32,
}

/// What went wrong reaching a path, which is always the tar's fault rather than the
/// caller's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PathError {
    /// An empty path, or one that is only separators.
    Empty,
}

impl Tree {
    pub fn new() -> Tree {
        Tree {
            root: Directory::new(Meta::implied_dir()),
            next_id: 0,
        }
    }

    /// A fresh inode identity, for a regular file that is not a hard link to one
    /// already in the tree.
    pub fn next_inode(&mut self) -> InodeId {
        let id = InodeId(self.next_id);
        self.next_id += 1;
        id
    }

    /// Put `node` at `path`, creating the directories above it that the tar did not
    /// mention. A directory landing on a directory keeps the children already there
    /// and takes the new metadata, because a tar may describe a directory after its
    /// contents; anything else replaces what was there.
    pub fn insert(&mut self, path: &[u8], node: Node) -> Result<(), PathError> {
        let (parent, name) = split(path)?;
        let dir = self.make_dir(parent)?;

        match (dir.entries.get_mut(name), node) {
            (Some(Node::Directory(existing)), Node::Directory(new)) => {
                existing.meta = new.meta;
                for (name, child) in new.entries {
                    existing.entries.insert(name, child);
                }
            }
            (_, node) => {
                dir.entries.insert(name.to_vec(), node);
            }
        }

        Ok(())
    }

    /// Take the node at `path` out of the tree, if it is there.
    pub fn remove(&mut self, path: &[u8]) -> Option<Node> {
        let (parent, name) = split(path).ok()?;
        self.dir_mut(parent)?.entries.remove(name)
    }

    pub fn get(&self, path: &[u8]) -> Option<&Node> {
        let (parent, name) = split(path).ok()?;
        self.dir(parent)?.entries.get(name)
    }

    /// The directory at `path`, where the empty path is the root.
    pub fn dir(&self, path: &[u8]) -> Option<&Directory> {
        let mut dir = &self.root;
        for name in components(path) {
            match dir.entries.get(name) {
                Some(Node::Directory(child)) => dir = child,
                _ => return None,
            }
        }
        Some(dir)
    }

    pub fn dir_mut(&mut self, path: &[u8]) -> Option<&mut Directory> {
        let mut dir = &mut self.root;
        for name in components(path) {
            match dir.entries.get_mut(name) {
                Some(Node::Directory(child)) => dir = child,
                _ => return None,
            }
        }
        Some(dir)
    }

    /// The directory at `path`, creating any part of it that is missing.
    pub fn make_dir(&mut self, path: &[u8]) -> Result<&mut Directory, PathError> {
        let mut dir = &mut self.root;
        for name in components(path) {
            let entry = dir
                .entries
                .entry(name.to_vec())
                .or_insert_with(|| Node::Directory(Directory::new(Meta::implied_dir())));

            // A tar may name a file and then name something under it. The file loses:
            // the later entry is the one that says what the layer ends up holding.
            if !entry.is_dir() {
                *entry = Node::Directory(Directory::new(Meta::implied_dir()));
            }

            dir = match entry {
                Node::Directory(child) => child,
                _ => unreachable!("just replaced with a directory"),
            };
        }

        Ok(dir)
    }

    /// How many paths point at each inode, which is an inode's `i_nlink`.
    pub fn link_counts(&self) -> std::collections::HashMap<InodeId, u32> {
        fn walk(dir: &Directory, counts: &mut std::collections::HashMap<InodeId, u32>) {
            for node in dir.entries.values() {
                match node {
                    Node::File { id, .. } => *counts.entry(*id).or_insert(0) += 1,
                    Node::Directory(child) => walk(child, counts),
                    _ => {}
                }
            }
        }

        let mut counts = std::collections::HashMap::new();
        walk(&self.root, &mut counts);
        counts
    }
}

impl Default for Tree {
    fn default() -> Tree {
        Tree::new()
    }
}

/// The components of a path, with the separators and any empty ones dropped. Paths
/// are bytes rather than `str` because a Linux filename is bytes, and a layer that
/// carries a name which is not UTF-8 is a layer that has to survive the round trip.
pub fn components(path: &[u8]) -> impl Iterator<Item = &[u8]> {
    path.split(|byte| *byte == b'/')
        .filter(|part| !part.is_empty())
}

/// A path split into the directory holding it and the name within.
fn split(path: &[u8]) -> Result<(&[u8], &[u8]), PathError> {
    let end = path
        .iter()
        .rposition(|byte| *byte != b'/')
        .ok_or(PathError::Empty)?;
    let path = &path[..=end];

    Ok(match path.iter().rposition(|byte| *byte == b'/') {
        Some(slash) => (&path[..slash], &path[slash + 1..]),
        None => (&[][..], path),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(id: u32) -> Node {
        Node::File {
            meta: Meta::implied_dir(),
            id: InodeId(id),
            data: Extent { offset: 0, len: 0 },
        }
    }

    #[test]
    fn splits_a_path_into_parent_and_name() {
        assert_eq!(split(b"a/b/c").unwrap(), (&b"a/b"[..], &b"c"[..]));
        assert_eq!(split(b"c").unwrap(), (&b""[..], &b"c"[..]));
        assert_eq!(split(b"a/b/").unwrap(), (&b"a"[..], &b"b"[..]));
        assert_eq!(split(b"").unwrap_err(), PathError::Empty);
        assert_eq!(split(b"//").unwrap_err(), PathError::Empty);
    }

    #[test]
    fn insert_creates_the_directories_a_tar_left_out() {
        let mut tree = Tree::new();
        tree.insert(b"usr/lib/libc.so", file(0)).unwrap();

        assert!(tree.dir(b"usr").is_some());
        assert!(tree.dir(b"usr/lib").is_some());
        assert!(matches!(
            tree.get(b"usr/lib/libc.so"),
            Some(Node::File { .. })
        ));
    }

    #[test]
    fn a_directory_named_late_keeps_what_is_already_under_it() {
        let mut tree = Tree::new();
        tree.insert(b"etc/passwd", file(0)).unwrap();

        let mut meta = Meta::implied_dir();
        meta.mode = 0o700;
        tree.insert(b"etc", Node::Directory(Directory::new(meta)))
            .unwrap();

        assert_eq!(tree.dir(b"etc").unwrap().meta.mode, 0o700);
        assert!(tree.get(b"etc/passwd").is_some());
    }

    #[test]
    fn link_counts_see_every_path_at_an_inode() {
        let mut tree = Tree::new();
        tree.insert(b"bin/busybox", file(7)).unwrap();
        tree.insert(b"bin/sh", file(7)).unwrap();
        tree.insert(b"bin/ls", file(8)).unwrap();

        let counts = tree.link_counts();
        assert_eq!(counts[&InodeId(7)], 2);
        assert_eq!(counts[&InodeId(8)], 1);
    }
}
