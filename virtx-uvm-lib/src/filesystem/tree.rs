//! The shape of a filesystem, held in memory.
//!
//! What every side of a conversion speaks: a [`Changeset`](super::Changeset) is read into
//! one, an [`Erofs`](super::Erofs) is written from one, and several layers merged are one
//! again -- so this belongs to none of them.
//!
//! It exists because the two ends disagree about order. A tar is a sequence: an entry is
//! whatever the entry before it left behind, and a later entry may delete or replace an
//! earlier one. An EROFS is a placement: an inode's name is its offset in the metadata
//! area, a directory entry names its children by that offset, and a hard link is one
//! inode counted from several directories. Nothing can be written until every entry is
//! known, so everything is read first.
//!
//! A file's data is its bytes or where in another file they already are -- see [`Data`].
//! A tree whose files hold their bytes costs memory as large as what the files hold: the
//! uncompressed size of a layer, while it is being converted. A merged image's holds none
//! of them.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::Arc;

use super::{Error, Result};

/// What every entry but a whiteout has. `mode` is permission and setuid bits only; the
/// type comes from the [`Entry`] variant, so the two cannot disagree.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Meta {
    pub mode: u16,
    pub uid: u32,
    pub gid: u32,
    pub mtime: i64,
    pub xattrs: BTreeMap<Vec<u8>, Vec<u8>>,
}

/// What a regular file's data is in a [`Tree`]: its bytes, or where in another file they
/// already are.
///
/// A tree read from an archive or an ext4 holds the bytes. One read out of an EROFS layer
/// to be merged points into that layer instead (see `erofs::read_spanned`), so that the
/// image written of it can point there too rather than carry a copy. Either is a whole
/// tree: a span's bytes can always be read, from the file it names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Data {
    Buffer(Vec<u8>),
    /// `len` bytes from byte `offset` of `file`.
    Span {
        file: Arc<Path>,
        offset: u64,
        len: u64,
    },
}

impl Data {
    /// How many bytes the file holds, which is its `i_size` whatever the data is.
    pub fn size(&self) -> u64 {
        match self {
            Data::Buffer(bytes) => bytes.len() as u64,
            Data::Span { len, .. } => *len,
        }
    }

    /// The bytes: held, or read out of the file they are in.
    pub fn contents(&self) -> Result<Cow<'_, [u8]>> {
        match self {
            Data::Buffer(bytes) => Ok(Cow::Borrowed(bytes)),
            Data::Span { file, offset, len } => {
                let reading = |e| Error::io(format!("reading {}", file.display()), e);
                let mut opened = File::open(file).map_err(reading)?;
                opened.seek(SeekFrom::Start(*offset)).map_err(reading)?;
                // Asked of the file rather than allocated up front, so a length the file
                // does not have comes back short instead of asking for the memory.
                let mut bytes = Vec::new();
                opened.take(*len).read_to_end(&mut bytes).map_err(reading)?;
                if bytes.len() as u64 != *len {
                    return Err(Error::io(
                        format!("reading {}", file.display()),
                        std::io::Error::new(
                            std::io::ErrorKind::UnexpectedEof,
                            format!("{len} bytes at {offset} run past its end"),
                        ),
                    ));
                }
                Ok(Cow::Owned(bytes))
            }
        }
    }
}

/// What an inode is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Entry {
    File {
        meta: Meta,
        data: Data,
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
    /// This path is deleted from the layers below. Only ever in a changeset: a merged
    /// tree has applied them all.
    Whiteout,
}

/// An inode of a tree, which is what a path names. Every path has its own, except the
/// paths [linked](Tree::link) to one another, which share one. Only a tree hands these
/// out.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Inode(u32);

/// A filesystem: one layer's, or several merged.
///
/// Paths and inodes are held apart, as a filesystem holds them: the paths name inodes,
/// and an inode's entry is held once however many paths name it. An inode no path names
/// any more -- one a later entry replaced -- is kept, and never walked.
#[derive(Debug)]
pub struct Tree {
    root: Node,
    /// Indexed by [`Inode`]. The root is inode 0.
    inodes: Vec<Entry>,
}

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
}

impl Tree {
    pub fn new() -> Tree {
        Tree {
            root: Node::new(Inode(0)),
            inodes: vec![implied_dir()],
        }
    }

    /// Put `entry` at `path`, creating the directories above it the archive did not name.
    ///
    /// A directory onto a directory keeps what is under it and takes the new metadata,
    /// because an archive may name a directory after its contents. Anything else replaces
    /// what was there, as a later entry in an archive does. The empty path is the root,
    /// which can only be a directory.
    pub fn insert(&mut self, path: &[u8], entry: Entry) -> Result<()> {
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
                    update_dir(&mut self.inodes[0], meta, opaque);
                    Ok(())
                }
                _ => Err(malformed(
                    path,
                    "puts something other than a directory at the root",
                )),
            };
        };

        let dir = make_dir(&mut self.root, &mut self.inodes, &names);
        if let Some(node) = dir.children.get(name)
            && let (Entry::Directory { .. }, Entry::Directory { .. }) =
                (&self.inodes[node.inode.0 as usize], &entry)
        {
            let Entry::Directory { meta, opaque } = entry else {
                unreachable!("just matched")
            };
            update_dir(&mut self.inodes[node.inode.0 as usize], meta, opaque);
        } else {
            let inode = push(&mut self.inodes, entry);
            dir.children.insert(name.to_vec(), Node::new(inode));
        }
        Ok(())
    }

    /// Make `path` a hard link to `target`: one more name for the inode already there.
    pub fn link(&mut self, path: &[u8], target: &[u8]) -> Result<()> {
        let inode = match self.get(target)? {
            Some(node) if matches!(self.inodes[node.inode.0 as usize], Entry::File { .. }) => {
                node.inode
            }
            _ => return Err(malformed(path, "links to something that is not a file")),
        };
        let mut names = components(path)?;
        let Some(name) = names.pop() else {
            return Err(malformed(path, "links the root"));
        };
        make_dir(&mut self.root, &mut self.inodes, &names)
            .children
            .insert(name.to_vec(), Node::new(inode));
        Ok(())
    }

    /// Make the directory at `path` opaque, creating it if the archive has not named it
    /// yet -- the marker may come before the directory does.
    pub fn opaque(&mut self, path: &[u8]) -> Result<()> {
        let names = components(path)?;
        let inode = make_dir(&mut self.root, &mut self.inodes, &names).inode;
        if let Entry::Directory { opaque, .. } = &mut self.inodes[inode.0 as usize] {
            *opaque = true;
        }
        Ok(())
    }

    /// Every path with the inode it names and that inode's entry, the root first and each
    /// directory before what is in it, siblings by name.
    pub fn walk(&self) -> Vec<(Vec<u8>, Inode, &Entry)> {
        fn visit<'a>(
            node: &Node,
            inodes: &'a [Entry],
            path: Vec<u8>,
            out: &mut Vec<(Vec<u8>, Inode, &'a Entry)>,
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

    /// The directory at `path` and everything under it, as a tree of its own with that
    /// directory as its root; the rest is dropped. `None` if there is no directory there.
    ///
    /// Only the inodes the subtree names come with it, so a file hard-linked from both
    /// inside and outside comes with the paths inside alone.
    pub fn into_subtree(mut self, path: &[u8]) -> Option<Tree> {
        let mut names = components(path).ok()?;
        let root = match names.pop() {
            None => return Some(self),
            Some(name) => {
                let mut parent = &mut self.root;
                for name in &names {
                    parent = parent.children.get_mut(*name)?;
                }
                parent.children.remove(name)?
            }
        };
        if !matches!(self.inodes[root.inode.0 as usize], Entry::Directory { .. }) {
            return None;
        }

        // Renumbered from 0 in the order they are reached, which puts the new root first.
        // Each is taken out of the old table once, however many paths name it.
        let mut old = self.inodes.into_iter().map(Some).collect::<Vec<_>>();
        let mut inodes = Vec::new();
        let mut renumbered = HashMap::new();
        let mut root = root;
        renumber(&mut root, &mut |inode| {
            *renumbered.entry(inode).or_insert_with(|| {
                let entry = old[inode.0 as usize].take().expect("taken once per inode");
                push(&mut inodes, entry)
            })
        });

        Some(Tree { root, inodes })
    }

    /// Put `other` at `path`, its root becoming the directory there, creating the
    /// directories above it that are missing. Whatever was at `path` is replaced, as
    /// [`insert`](Tree::insert) replaces it; the empty path replaces the whole tree.
    pub fn graft(&mut self, path: &[u8], other: Tree) -> Result<()> {
        let mut names = components(path)?;
        let Some(name) = names.pop() else {
            *self = other;
            return Ok(());
        };

        // `other`'s inodes go after this tree's own, so every one of them moves by as many
        // as there are here.
        let offset = self.inodes.len() as u32;
        let mut root = other.root;
        renumber(&mut root, &mut |inode| Inode(inode.0 + offset));
        self.inodes.extend(other.inodes);

        make_dir(&mut self.root, &mut self.inodes, &names)
            .children
            .insert(name.to_vec(), root);
        Ok(())
    }

    /// Take `path` and everything under it out of the tree, and say whether anything was
    /// there to take. A path that is not there costs nothing, so a caller can name every
    /// path it might want gone. The root is never taken.
    pub fn prune(&mut self, path: &[u8]) -> bool {
        let Ok(mut names) = components(path) else {
            return false;
        };
        let Some(name) = names.pop() else {
            return false;
        };
        let mut dir = &mut self.root;
        for name in &names {
            match dir.children.get_mut(*name) {
                Some(child) => dir = child,
                None => return false,
            }
        }
        dir.children.remove(name).is_some()
    }

    /// The filesystem `layers` describe, bottom first: each one's changes applied over
    /// those below it.
    ///
    /// Later layers win: a file replaces what is under its name, a whiteout deletes it,
    /// and an opaque directory empties what was below before anything of its own goes in.
    /// What comes back is a filesystem and not a changeset, so neither marker survives --
    /// not even from a lone layer, where a whiteout would otherwise be booted as a device
    /// numbered 0:0.
    ///
    /// A hard link within a layer stays one inode. Every directory takes the metadata of
    /// the topmost layer that names it, the root included.
    pub fn merge(layers: impl IntoIterator<Item = Tree>) -> Tree {
        let mut merged = Tree::new();
        for layer in layers {
            merged.overlay(layer);
        }
        merged
    }

    /// Apply `upper`, as one layer, over this tree.
    fn overlay(&mut self, upper: Tree) {
        let mut inodes = upper.inodes.into_iter().map(Some).collect::<Vec<_>>();
        let mut moved = HashMap::new();

        let Some(Entry::Directory { meta, opaque }) = inodes[0].take() else {
            unreachable!("a tree's root is a directory")
        };
        self.inodes[0] = Entry::Directory {
            meta,
            opaque: false,
        };
        if opaque {
            self.root.children.clear();
        }

        overlay(
            &mut self.root,
            &mut self.inodes,
            upper.root,
            &mut inodes,
            &mut moved,
        );
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

impl Default for Tree {
    fn default() -> Tree {
        Tree::new()
    }
}

/// Apply the directory `upper` over `lower`, taking each of `upper`'s inodes out of
/// `from` into `into` as it is reached. `moved` is where each one went, so the second
/// path to a hard-linked file names the inode the first one moved.
fn overlay(
    lower: &mut Node,
    into: &mut Vec<Entry>,
    upper: Node,
    from: &mut [Option<Entry>],
    moved: &mut HashMap<Inode, Inode>,
) {
    for (name, child) in upper.children {
        if let Some(&inode) = moved.get(&child.inode) {
            lower.children.insert(name, Node::new(inode));
            continue;
        }

        match from[child.inode.0 as usize]
            .take()
            .expect("an inode is taken once, and then found in `moved`")
        {
            // A deletion and not an entry: what it stood over goes, and it does not take
            // its place.
            Entry::Whiteout => {
                lower.children.remove(&name);
            }

            Entry::Directory { meta, opaque } => {
                // A directory over a directory keeps what is under it, unless it said not
                // to. Anything else it lands on is replaced: a layer that puts a directory
                // where a file was has said which of the two the image ends up with.
                let mut dir = match lower.children.remove(&name) {
                    Some(below)
                        if !opaque
                            && matches!(into[below.inode.0 as usize], Entry::Directory { .. }) =>
                    {
                        below
                    }
                    _ => Node::new(push(into, implied_dir())),
                };
                into[dir.inode.0 as usize] = Entry::Directory {
                    meta,
                    opaque: false,
                };

                // Recursed into even when there was nothing below, because the layer's own
                // markers are dropped on the way through whatever else it holds.
                overlay(&mut dir, into, child, from, moved);
                lower.children.insert(name, dir);
            }

            entry => {
                let inode = push(into, entry);
                moved.insert(child.inode, inode);
                lower.children.insert(name, Node::new(inode));
            }
        }
    }
}

/// Give every path under `node`, `node` included, the inode `map` says its own becomes.
fn renumber(node: &mut Node, map: &mut impl FnMut(Inode) -> Inode) {
    node.inode = map(node.inode);
    for child in node.children.values_mut() {
        renumber(child, map);
    }
}

/// A new inode holding `entry`.
fn push(inodes: &mut Vec<Entry>, entry: Entry) -> Inode {
    inodes.push(entry);
    Inode(inodes.len() as u32 - 1)
}

/// The directory at `names` under `root`, making what is missing. Something other than a
/// directory along the way is replaced: an archive may name a file and then something
/// under it, and the later entry is the one that says what the layer holds.
fn make_dir<'a>(root: &'a mut Node, inodes: &mut Vec<Entry>, names: &[&[u8]]) -> &'a mut Node {
    let mut dir = root;
    for name in names {
        let is_dir = dir
            .children
            .get(*name)
            .is_some_and(|child| matches!(inodes[child.inode.0 as usize], Entry::Directory { .. }));
        if !is_dir {
            let inode = push(inodes, implied_dir());
            dir.children.insert(name.to_vec(), Node::new(inode));
        }
        dir = dir.children.get_mut(*name).expect("just made");
    }
    dir
}

/// Give a directory `meta`, and make it opaque if `opaque` -- never the other way, since
/// a directory named again does not stop hiding what is below it.
fn update_dir(entry: &mut Entry, meta: Meta, opaque: bool) {
    if let Entry::Directory {
        meta: old,
        opaque: was,
    } = entry
    {
        *old = meta;
        *was |= opaque;
    }
}

/// What a directory gets that the archive never named, because archives may leave them
/// out.
fn implied_dir() -> Entry {
    Entry::Directory {
        meta: Meta {
            mode: 0o755,
            ..Meta::default()
        },
        opaque: false,
    }
}

/// The names along a path. Empty ones and `.` are dropped, so `./etc/` is `etc`; `..` is
/// refused, being a way out of the tree. Bytes rather than `str`, because a Linux
/// filename is bytes and one that is not UTF-8 has to survive the round trip.
fn components(path: &[u8]) -> Result<Vec<&[u8]>> {
    let mut names = Vec::new();
    for name in path.split(|byte| *byte == b'/') {
        match name {
            b"" | b"." => {}
            b".." => return Err(malformed(path, "climbs out of the root")),
            name => names.push(name),
        }
    }
    Ok(names)
}

fn malformed(path: &[u8], what: &str) -> Error {
    Error::Path(format!("{} {what}", String::from_utf8_lossy(path)))
}
