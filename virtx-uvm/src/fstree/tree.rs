//! The shape of a filesystem, held in memory: what every form is read into and written
//! out of, so that no form has to know about any other.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::Arc;

use oci_spec::image::{Digest, Sha256Digest};
use sha2::{Digest as _, Sha256};

use super::{Error, Result};

/// What every entry but a whiteout has. `mode` is permission and setuid bits only; the
/// type comes from the [`Entry`] variant, so the two cannot disagree.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Meta {
    pub mode: u16,
    pub uid: u32,
    pub gid: u32,
    /// Last read.
    pub atime: Timestamp,
    /// Last written: the one every form keeps.
    pub mtime: Timestamp,
    /// Last changed, contents or metadata.
    pub ctime: Timestamp,
    /// Made.
    pub crtime: Timestamp,
    pub xattrs: BTreeMap<Vec<u8>, Vec<u8>>,
}

/// A point in time, as the seconds since the Unix epoch -- before it, for a negative one --
/// and the nanoseconds past that second.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp {
    pub secs: i64,
    pub nanos: u32,
}

impl Timestamp {
    pub const ZERO: Timestamp = Timestamp { secs: 0, nanos: 0 };
}

/// A file's contents left where they are: `len` bytes from block `block` of the EROFS
/// image at `path`. What a tree merged from layers holds in place of the bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    pub path: Arc<Path>,
    pub block: u32,
    pub len: u64,
}

/// What an inode is. `D` is a file's contents: the bytes, or a [`Span`] of them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Entry<D = Vec<u8>> {
    File {
        meta: Meta,
        data: D,
    },
    /// `opaque`: nothing from the layers below shows through it.
    Directory {
        meta: Meta,
        opaque: bool,
    },
    /// Its `meta.mode` is always `0o777`, whatever it was put in with: Linux gives a
    /// symlink no permissions of its own, and says `lrwxrwxrwx` of every one. Held as that,
    /// a symlink reads back the same whichever form it went through.
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
    /// A name a process bound to listen on. Nothing listens once the process is gone, but
    /// the name is still there, and the next one to bind it has to take it away first.
    Socket(Meta),
    /// This path is deleted from the layers below. Only ever in a changeset: a merged
    /// tree has applied them all.
    Whiteout,
}

/// What a directory holds, each entry by its name there.
pub type Entries<'a, D = Vec<u8>> = Vec<(&'a [u8], &'a Entry<D>)>;

/// An inode of a tree, which is what a path names. Every path has its own, except the
/// paths [linked](FsTree::link) to one another, which share one. Only a tree hands these
/// out.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Inode(u32);

/// One path: the inode it names, and the paths under it if it is a directory.
#[derive(Debug)]
struct Node {
    inode: Inode,
    /// Sorted by name, which is the order EROFS wants dirents in -- and what makes an
    /// image written from a tree the same bytes every time. Empty for anything but a
    /// directory.
    children: BTreeMap<Vec<u8>, Node>,
}

impl Node {
    fn new(inode: Inode) -> Node {
        Node {
            inode,
            children: BTreeMap::new(),
        }
    }

    /// The directory at `names` under this one, making what is missing. Something other
    /// than a directory along the way is replaced: an archive may name a file and then
    /// something under it, and the later entry is the one that says what the layer holds.
    ///
    /// On `Node` and not the tree, so that the directory it answers with and `inodes` can
    /// be borrowed at once.
    fn make_dir<D>(&mut self, inodes: &mut Vec<Entry<D>>, names: &[&[u8]]) -> &mut Node {
        let mut dir = self;
        for name in names {
            let is_dir = dir.children.get(*name).is_some_and(|child| {
                matches!(inodes[child.inode.0 as usize], Entry::Directory { .. })
            });
            if !is_dir {
                inodes.push(Entry::IMPLIED_DIR);
                let inode = Inode(inodes.len() as u32 - 1);
                dir.children.insert(name.to_vec(), Node::new(inode));
            }
            dir = dir.children.get_mut(*name).expect("just made");
        }
        dir
    }

    /// Lay the directory `upper` over this one. `upper`'s inodes are already in `inodes`,
    /// `base` past where its own tree numbered them.
    ///
    /// A directory takes `upper`'s entry by swapping it into its own inode, so the root
    /// stays inode 0; what it swaps out is named by nothing any more.
    fn stack<D>(&mut self, upper: Node, base: u32, inodes: &mut [Entry<D>]) {
        let at = |node: &Node| node.inode.0 as usize;
        inodes.swap(at(self), at(&upper) + base as usize);
        if let Entry::Directory { opaque, .. } = &mut inodes[at(self)] {
            if *opaque {
                self.children.clear();
            }
            *opaque = false;
        }

        for (name, child) in upper.children {
            let inode = Inode(child.inode.0 + base);
            match &inodes[inode.0 as usize] {
                Entry::Whiteout => {
                    self.children.remove(&name);
                }
                Entry::Directory { .. } => {
                    let is_dir = self
                        .children
                        .get(&name)
                        .is_some_and(|node| matches!(inodes[at(node)], Entry::Directory { .. }));
                    if !is_dir {
                        self.children.insert(name.clone(), Node::new(inode));
                    }
                    self.children
                        .get_mut(&name)
                        .expect("just made")
                        .stack(child, base, inodes);
                }
                _ => {
                    self.children.insert(name, Node::new(inode));
                }
            }
        }
    }
}

impl<D> Entry<D> {
    /// What a directory gets that the archive never named, because archives may leave them
    /// out.
    const IMPLIED_DIR: Entry<D> = Entry::Directory {
        meta: Meta {
            mode: 0o755,
            uid: 0,
            gid: 0,
            atime: Timestamp::ZERO,
            mtime: Timestamp::ZERO,
            ctime: Timestamp::ZERO,
            crtime: Timestamp::ZERO,
            xattrs: BTreeMap::new(),
        },
        opaque: false,
    };
}

/// A filesystem: one layer's, or several merged.
///
/// Paths and inodes are held apart, as a filesystem holds them: the paths name inodes,
/// and an inode's entry is held once however many paths name it. An inode no path names
/// any more -- one a later entry replaced -- is kept, and never walked.
#[derive(Debug)]
pub struct FsTree<D = Vec<u8>> {
    root: Node,
    /// Indexed by [`Inode`]. The root is inode 0.
    inodes: Vec<Entry<D>>,
}

impl<D> FsTree<D> {
    pub fn new() -> FsTree<D> {
        FsTree {
            root: Node::new(Inode(0)),
            inodes: vec![Entry::IMPLIED_DIR],
        }
    }

    /// Put `entry` at `path`, creating the directories above it the archive did not name.
    ///
    /// A directory onto a directory keeps what is under it and takes the new metadata,
    /// because an archive may name a directory after its contents. Anything else replaces
    /// what was there, as a later entry in an archive does. The empty path is the root,
    /// which can only be a directory.
    pub fn insert(&mut self, path: &[u8], entry: Entry<D>) -> Result<()> {
        let entry = match entry {
            Entry::Symlink { mut meta, target } => {
                meta.mode = 0o777;
                Entry::Symlink { meta, target }
            }
            entry => entry,
        };
        let mut names = components(path)?;
        let Some(name) = names.pop() else {
            return match entry {
                Entry::Directory { meta, opaque } => {
                    if let Entry::Directory {
                        meta: old,
                        opaque: was,
                    } = &mut self.inodes[0]
                    {
                        *old = meta;
                        *was |= opaque;
                    }
                    Ok(())
                }
                _ => Err(Error::Path(format!(
                    "{} puts something other than a directory at the root",
                    String::from_utf8_lossy(path)
                ))),
            };
        };

        let dir = self.root.make_dir(&mut self.inodes, &names);
        let existing = dir.children.get(name).map(|node| node.inode.0 as usize);
        match (existing.map(|i| &mut self.inodes[i]), entry) {
            // A directory onto a directory takes the new metadata, and becomes opaque if
            // the new one is -- never the other way, since a directory named again does not
            // stop hiding what is below it.
            (
                Some(Entry::Directory {
                    meta: old,
                    opaque: was,
                }),
                Entry::Directory { meta, opaque },
            ) => {
                *old = meta;
                *was |= opaque;
            }
            (_, entry) => {
                self.inodes.push(entry);
                let inode = Inode(self.inodes.len() as u32 - 1);
                dir.children.insert(name.to_vec(), Node::new(inode));
            }
        }
        Ok(())
    }

    /// Make `path` a hard link to `target`: one more name for the inode already there.
    pub fn link(&mut self, path: &[u8], target: &[u8]) -> Result<()> {
        let inode = match self.get(target)? {
            Some(node) if matches!(self.inodes[node.inode.0 as usize], Entry::File { .. }) => {
                node.inode
            }
            _ => {
                return Err(Error::Path(format!(
                    "{} links to something that is not a file",
                    String::from_utf8_lossy(path)
                )));
            }
        };
        let mut names = components(path)?;
        let Some(name) = names.pop() else {
            return Err(Error::Path(format!(
                "{} links the root",
                String::from_utf8_lossy(path)
            )));
        };
        self.root
            .make_dir(&mut self.inodes, &names)
            .children
            .insert(name.to_vec(), Node::new(inode));
        Ok(())
    }

    /// Make the directory at `path` opaque, creating it if the archive has not named it
    /// yet -- the marker may come before the directory does.
    pub fn opaque(&mut self, path: &[u8]) -> Result<()> {
        let names = components(path)?;
        let inode = self.root.make_dir(&mut self.inodes, &names).inode;
        if let Entry::Directory { opaque, .. } = &mut self.inodes[inode.0 as usize] {
            *opaque = true;
        }
        Ok(())
    }

    /// Every path with the inode it names and that inode's entry, the root first and each
    /// directory before what is in it, siblings by name.
    pub fn walk(&self) -> Vec<(Vec<u8>, Inode, &Entry<D>)> {
        fn visit<'a, D>(
            node: &Node,
            inodes: &'a [Entry<D>],
            path: Vec<u8>,
            out: &mut Vec<(Vec<u8>, Inode, &'a Entry<D>)>,
        ) {
            out.push((path.clone(), node.inode, &inodes[node.inode.0 as usize]));
            for (name, child) in &node.children {
                let mut path = path.clone();
                if !path.is_empty() {
                    path.push(b'/');
                }
                path.extend_from_slice(name);
                visit(child, inodes, path, out);
            }
        }

        let mut out = Vec::new();
        visit(&self.root, &self.inodes, Vec::new(), &mut out);
        out
    }

    /// Take `path` out of the tree, and everything under it if it is a directory. Whether
    /// anything was there.
    ///
    /// Only the name goes: a file hard-linked from elsewhere is still there under its other
    /// names. The root cannot be taken out.
    pub fn prune(&mut self, path: &[u8]) -> Result<bool> {
        let mut names = components(path)?;
        let Some(name) = names.pop() else {
            return Err(Error::Path(
                "the root cannot be taken out of a tree".to_string(),
            ));
        };
        let mut dir = &mut self.root;
        for name in names {
            match dir.children.get_mut(name) {
                Some(child) => dir = child,
                None => return Ok(false),
            }
        }
        Ok(dir.children.remove(name).is_some())
    }

    /// The tree under the directory at `path`, with that directory as its root: everything
    /// else is gone. `None` if nothing is there, or something that is not a directory.
    ///
    /// Its paths keep the inodes they named, so what was hard-linked under it still is.
    pub fn subtree(mut self, path: &[u8]) -> Result<Option<FsTree<D>>> {
        let mut dir = &mut self.root;
        for name in components(path)? {
            match dir.children.get_mut(name) {
                Some(child) => dir = child,
                None => return Ok(None),
            }
        }
        let mut root = std::mem::replace(dir, Node::new(Inode(0)));
        if !matches!(self.inodes[root.inode.0 as usize], Entry::Directory { .. }) {
            return Ok(None);
        }
        // The root is inode 0. What that was is the old root's, which no path under this
        // directory names.
        self.inodes.swap(0, root.inode.0 as usize);
        root.inode = Inode(0);
        Ok(Some(FsTree {
            root,
            inodes: self.inodes,
        }))
    }

    /// Take out every attribute under `trusted.overlay.`: what overlayfs wrote about the
    /// mount a session's upper layer was part of -- `origin`, `impure`, `nlink` and the rest
    /// -- which means nothing once that mount is gone. Whether a directory is opaque is the
    /// tree's own flag, and stays.
    pub fn strip_overlay(&mut self) {
        for entry in &mut self.inodes {
            let meta = match entry {
                Entry::File { meta, .. }
                | Entry::Directory { meta, .. }
                | Entry::Symlink { meta, .. }
                | Entry::Device { meta, .. }
                | Entry::Fifo(meta)
                | Entry::Socket(meta) => meta,
                Entry::Whiteout => continue,
            };
            meta.xattrs
                .retain(|name, _| !name.starts_with(b"trusted.overlay."));
        }
    }

    /// Lay `upper`, the layer above, over this tree, as overlayfs shows the one over the
    /// other: a whiteout takes the path away, an opaque directory hides everything below
    /// it, a directory over a directory keeps what is under the lower one, and anything
    /// else replaces what was there. Nothing of `upper`'s whiteouts or opaque flags is
    /// left, so a tree stacked from layers is a filesystem and not a changeset.
    ///
    /// Paths hard-linked in `upper` stay linked to one another: its inodes keep their
    /// numbering, moved past this tree's.
    pub fn stack(&mut self, upper: FsTree<D>) {
        let base = self.inodes.len() as u32;
        self.inodes.extend(upper.inodes);
        self.root.stack(upper.root, base, &mut self.inodes);
    }

    /// What is at `path`, if anything.
    pub fn entry(&self, path: &[u8]) -> Result<Option<&Entry<D>>> {
        Ok(self
            .get(path)?
            .map(|node| &self.inodes[node.inode.0 as usize]))
    }

    /// What is in the directory at `path`, by name: `None` if nothing is there, or
    /// something that is not a directory.
    pub fn entries(&self, path: &[u8]) -> Result<Option<Entries<'_, D>>> {
        let Some(node) = self.get(path)? else {
            return Ok(None);
        };
        if !matches!(self.inodes[node.inode.0 as usize], Entry::Directory { .. }) {
            return Ok(None);
        }
        Ok(Some(
            node.children
                .iter()
                .map(|(name, child)| (name.as_slice(), &self.inodes[child.inode.0 as usize]))
                .collect(),
        ))
    }

    /// Move what is at `from`, and everything under it, to `to`, replacing what is there.
    /// Whether anything was at `from`. Refused when `to`'s parent is not a directory that
    /// is there, or `to` is in `from`.
    pub fn rename(&mut self, from: &[u8], to: &[u8]) -> Result<bool> {
        let from_names = components(from)?;
        let mut to_names = components(to)?;
        let Some(to_name) = to_names.pop() else {
            return Err(Error::Path("the root cannot be moved onto".to_string()));
        };
        if from_names.is_empty() {
            return Err(Error::Path("the root cannot be moved".to_string()));
        }
        if to_names.len() >= from_names.len() && to_names[..from_names.len()] == from_names[..] {
            return Err(Error::Path(format!(
                "{} is inside {}",
                String::from_utf8_lossy(to),
                String::from_utf8_lossy(from)
            )));
        }
        let parent = self.get(&to_names.join(&b'/'))?.map(|node| node.inode);
        match parent {
            Some(inode) if matches!(self.inodes[inode.0 as usize], Entry::Directory { .. }) => {}
            _ => {
                return Err(Error::Path(format!(
                    "{} is not in a directory",
                    String::from_utf8_lossy(to)
                )));
            }
        }
        let (last, dirs) = from_names.split_last().expect("not empty");
        let mut dir = &mut self.root;
        for name in dirs {
            match dir.children.get_mut(*name) {
                Some(child) => dir = child,
                None => return Ok(false),
            }
        }
        let Some(moved) = dir.children.remove(*last) else {
            return Ok(false);
        };
        let mut dir = &mut self.root;
        for name in to_names {
            dir = dir.children.get_mut(name).expect("checked above");
        }
        dir.children.insert(to_name.to_vec(), moved);
        Ok(true)
    }

    fn get(&self, path: &[u8]) -> Result<Option<&Node>> {
        let mut node = &self.root;
        for name in components(path)? {
            match node.children.get(name) {
                Some(child) => node = child,
                None => return Ok(None),
            }
        }
        Ok(Some(node))
    }
}

impl<D> Default for FsTree<D> {
    fn default() -> FsTree<D> {
        FsTree::new()
    }
}

/// Everything the tree says, and nothing about how it came to say it: two trees with the
/// same paths, entries and metadata have the same digest whatever order they were built
/// in, and two that differ in anything at all -- a time, an xattr, a byte -- do not.
///
/// What is hashed is the walk, one record a path: the path, what is there, and its
/// metadata, every field of it. Each record says how long anything of no fixed length is
/// before it says it, so no two trees put the same bytes in front of the hash. A path that
/// names an inode an earlier path did is a link to that path, by its place in the walk:
/// inode numbers are the order a tree was built in, and not what it holds.
impl FsTree {
    pub fn digest(&self) -> Digest {
        let mut hasher = Sha256::new();
        // Every field with its length before it -- the fixed-width ones too, which would
        // not need it, so that there is one way a field goes in.
        let mut bytes = |bytes: &[u8]| {
            hasher.update((bytes.len() as u64).to_le_bytes());
            hasher.update(bytes);
        };
        let mut first: HashMap<Inode, u64> = HashMap::new();
        for (index, (path, inode, entry)) in self.walk().into_iter().enumerate() {
            bytes(&path);
            if let Some(earlier) = first.get(&inode) {
                bytes(b"link");
                bytes(&earlier.to_le_bytes());
                continue;
            }
            first.insert(inode, index as u64);

            let meta = match entry {
                Entry::File { meta, data } => {
                    bytes(b"file");
                    bytes(data);
                    meta
                }
                Entry::Directory { meta, opaque } => {
                    bytes(b"directory");
                    bytes(&[u8::from(*opaque)]);
                    meta
                }
                Entry::Symlink { meta, target } => {
                    bytes(b"symlink");
                    bytes(target);
                    meta
                }
                Entry::Device {
                    meta,
                    block,
                    major,
                    minor,
                } => {
                    bytes(b"device");
                    bytes(&[u8::from(*block)]);
                    bytes(&major.to_le_bytes());
                    bytes(&minor.to_le_bytes());
                    meta
                }
                Entry::Fifo(meta) => {
                    bytes(b"fifo");
                    meta
                }
                Entry::Socket(meta) => {
                    bytes(b"socket");
                    meta
                }
                Entry::Whiteout => {
                    bytes(b"whiteout");
                    continue;
                }
            };
            bytes(&meta.mode.to_le_bytes());
            bytes(&meta.uid.to_le_bytes());
            bytes(&meta.gid.to_le_bytes());
            for time in [meta.atime, meta.mtime, meta.ctime, meta.crtime] {
                bytes(&time.secs.to_le_bytes());
                bytes(&time.nanos.to_le_bytes());
            }
            bytes(&(meta.xattrs.len() as u64).to_le_bytes());
            for (name, value) in &meta.xattrs {
                bytes(name);
                bytes(value);
            }
        }

        format!("{:x}", hasher.finalize())
            .parse::<Sha256Digest>()
            .expect("sha2 writes 64 lowercase hex characters")
            .into()
    }
}

/// The names along a path. Empty ones and `.` are dropped, so `./etc/` is `etc`; `..`
/// is refused, being a way out of the tree.
fn components(path: &[u8]) -> Result<Vec<&[u8]>> {
    let mut names = Vec::new();
    for name in path.split(|byte| *byte == b'/') {
        match name {
            b"" | b"." => {}
            b".." => {
                return Err(Error::Path(format!(
                    "{} climbs out of the root",
                    String::from_utf8_lossy(path)
                )));
            }
            name => names.push(name),
        }
    }
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(data: &[u8]) -> Entry {
        Entry::File {
            meta: Meta::default(),
            data: data.to_vec(),
        }
    }

    fn dir(uid: u32, opaque: bool) -> Entry {
        Entry::Directory {
            meta: Meta {
                uid,
                ..Meta::default()
            },
            opaque,
        }
    }

    fn paths(tree: &FsTree) -> Vec<String> {
        tree.walk()
            .into_iter()
            .map(|(path, ..)| String::from_utf8(path).unwrap())
            .collect()
    }

    fn at<'a>(tree: &'a FsTree, path: &str) -> &'a Entry {
        let inode = tree.get(path.as_bytes()).unwrap().unwrap().inode;
        &tree.inodes[inode.0 as usize]
    }

    fn lower() -> FsTree {
        let mut tree = FsTree::new();
        tree.insert(b"etc", dir(7, false)).unwrap();
        tree.insert(b"etc/hostname", file(b"lower")).unwrap();
        tree.insert(b"etc/passwd", file(b"root")).unwrap();
        tree.insert(b"var", dir(0, false)).unwrap();
        tree.insert(b"var/cache/old", file(b"old")).unwrap();
        tree.insert(b"bin", file(b"not a directory yet")).unwrap();
        tree
    }

    #[test]
    fn a_path_is_looked_up_and_a_directory_listed() {
        let tree = lower();
        assert_eq!(tree.entry(b"etc/passwd").unwrap(), Some(&file(b"root")));
        assert_eq!(tree.entry(b"etc/none").unwrap(), None);
        assert!(matches!(
            tree.entry(b"").unwrap(),
            Some(Entry::Directory { .. })
        ));
        assert!(tree.entry(b"../x").is_err());

        let listed: Vec<_> = tree
            .entries(b"etc")
            .unwrap()
            .unwrap()
            .into_iter()
            .map(|(name, _)| name.to_vec())
            .collect();
        assert_eq!(listed, [b"hostname".to_vec(), b"passwd".to_vec()]);
        assert!(tree.entries(b"bin").unwrap().is_none());
        assert!(tree.entries(b"none").unwrap().is_none());
    }

    #[test]
    fn a_rename_moves_a_tree_and_replaces_what_was_there() {
        let mut tree = lower();
        assert!(tree.rename(b"var", b"etc/var").unwrap());
        assert!(tree.rename(b"etc/hostname", b"bin").unwrap());
        assert_eq!(
            paths(&tree),
            [
                "",
                "bin",
                "etc",
                "etc/passwd",
                "etc/var",
                "etc/var/cache",
                "etc/var/cache/old"
            ]
        );
        assert_eq!(at(&tree, "bin"), &file(b"lower"));

        assert!(!tree.rename(b"none", b"x").unwrap());
        assert!(tree.rename(b"etc", b"etc/var/inside").is_err());
        assert!(tree.rename(b"etc", b"none/x").is_err());
        assert!(tree.rename(b"etc", b"bin/x").is_err());
        assert!(tree.rename(b"", b"x").is_err());
    }

    #[test]
    fn a_whiteout_takes_the_path_away() {
        let mut tree = lower();
        let mut upper = FsTree::new();
        upper.insert(b"etc/passwd", Entry::Whiteout).unwrap();
        upper.insert(b"var", Entry::Whiteout).unwrap();
        upper.insert(b"nothing/below", Entry::Whiteout).unwrap();

        tree.stack(upper);
        assert_eq!(paths(&tree), ["", "bin", "etc", "etc/hostname", "nothing"]);
    }

    #[test]
    fn an_opaque_directory_hides_what_is_below() {
        let mut tree = lower();
        let mut upper = FsTree::new();
        upper.insert(b"var", dir(3, true)).unwrap();
        upper.insert(b"var/new", file(b"new")).unwrap();

        tree.stack(upper);
        assert!(paths(&tree).contains(&"var/new".to_string()));
        assert!(!paths(&tree).contains(&"var/cache".to_string()));
        assert_eq!(at(&tree, "var"), &dir(3, false));
    }

    #[test]
    fn a_directory_over_a_directory_keeps_what_is_under_it() {
        let mut tree = lower();
        let mut upper = FsTree::new();
        upper.insert(b"etc", dir(9, false)).unwrap();
        upper.insert(b"etc/hostname", file(b"upper")).unwrap();
        upper.insert(b"bin/sh", file(b"sh")).unwrap();

        tree.stack(upper);
        assert_eq!(at(&tree, "etc"), &dir(9, false));
        assert_eq!(at(&tree, "etc/hostname"), &file(b"upper"));
        assert_eq!(at(&tree, "etc/passwd"), &file(b"root"));
        assert!(matches!(at(&tree, "bin"), Entry::Directory { .. }));
        assert_eq!(at(&tree, "bin/sh"), &file(b"sh"));
    }

    /// A layer that names `etc/motd` and never `etc` gives `etc` the default metadata, as
    /// Moby does.
    #[test]
    fn a_directory_never_named_takes_the_default_metadata() {
        let mut tree = lower();
        let mut upper = FsTree::new();
        upper.insert(b"etc/motd", file(b"hi")).unwrap();
        upper.opaque(b"var/cache").unwrap();

        tree.stack(upper);
        assert_eq!(at(&tree, "etc"), &Entry::IMPLIED_DIR);
        assert_eq!(at(&tree, "etc/passwd"), &file(b"root"));
        assert!(!paths(&tree).contains(&"var/cache/old".to_string()));
        assert!(paths(&tree).contains(&"var/cache".to_string()));
    }

    #[test]
    fn links_in_the_upper_stay_linked() {
        let mut tree = lower();
        let mut upper = FsTree::new();
        upper.insert(b"usr/a", file(b"same")).unwrap();
        upper.link(b"usr/b", b"usr/a").unwrap();

        tree.stack(upper);
        let a = tree.get(b"usr/a").unwrap().unwrap().inode;
        let b = tree.get(b"usr/b").unwrap().unwrap().inode;
        assert_eq!(a, b);
        assert_eq!(at(&tree, "usr/a"), &file(b"same"));
    }

    #[test]
    fn a_span_tree_stacks_as_a_bytes_tree_does() {
        let span = |block| Span {
            path: Arc::from(Path::new("layer.erofs")),
            block,
            len: 1,
        };
        let mut tree = FsTree::<Span>::new();
        tree.insert(
            b"a",
            Entry::File {
                meta: Meta::default(),
                data: span(1),
            },
        )
        .unwrap();
        let mut upper = FsTree::<Span>::new();
        upper
            .insert(
                b"a",
                Entry::File {
                    meta: Meta::default(),
                    data: span(2),
                },
            )
            .unwrap();

        tree.stack(upper);
        let inode = tree.get(b"a").unwrap().unwrap().inode;
        assert!(
            matches!(&tree.inodes[inode.0 as usize], Entry::File { data, .. } if data.block == 2)
        );
    }

    #[test]
    fn a_subtree_is_what_was_under_its_directory() {
        let mut tree = lower();
        tree.link(b"etc/hosts", b"etc/hostname").unwrap();
        let sub = tree.subtree(b"etc").unwrap().unwrap();
        assert_eq!(paths(&sub), ["", "hostname", "hosts", "passwd"]);
        assert_eq!(at(&sub, ""), &dir(7, false));
        assert_eq!(at(&sub, "hosts"), &file(b"lower"));
        let inode = |path: &str| sub.get(path.as_bytes()).unwrap().unwrap().inode;
        assert_eq!(inode("hosts"), inode("hostname"));

        assert!(lower().subtree(b"bin").unwrap().is_none());
        assert!(lower().subtree(b"nowhere").unwrap().is_none());
    }
}
