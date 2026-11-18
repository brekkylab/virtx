//! ext4, the filesystem a session's writable upper layer is.
//!
//! Read and written in overlayfs's spelling, which is how the guest left it and how it
//! will read it: a deletion is a character device numbered 0:0, and an opaque directory
//! carries `trusted.overlay.opaque`. Everything else overlayfs writes under
//! `trusted.overlay.` -- `origin`, `impure`, `nlink` and the rest -- is read as it is, so
//! an image read and written back is the image it was; a tree going on to be a layer has
//! it taken out ([`FsTree::strip_overlay`]).
//!
//! That holds only for an upper written without `redirect_dir` or `metacopy`, which keep
//! part of what changed on the lower: a renamed directory's contents, a file whose data was
//! never copied up. Read alone, such an upper is not the whole of a session's work. The
//! guest's init mounts its overlay without either.

mod reader;
mod writer;

use std::io::{BufWriter, Read, Seek, Write};

use self::reader::{Reader, walk};
use self::writer::Plan;
use super::{Error, FsTree, Result};

/// A file's contents left where they are in an ext4 image: those of inode `ino`, `len` bytes.
/// What a tree read for its metadata alone ([`Ext4::meta`]) holds in place of the bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ext4File {
    pub ino: u32,
    pub len: u64,
}

/// An ext4 image as it is read in parts: where its inodes are, and the blocks its journal
/// committed, read once ([`Ext4::open`]) and over again for each part. Each read is of the
/// image it was opened over, as it was then.
pub struct Ext4(Reader<()>);

impl Ext4 {
    /// `image`, an ext4 image from its byte zero, as [`FsTree::from_ext4`] reads one: what a
    /// guest killed midway left committed in its journal read over it, and refused where that
    /// refuses it.
    pub fn open(image: impl Read + Seek) -> Result<Ext4> {
        Ok(Ext4(Reader::open(image)?.with(())))
    }

    /// The tree in `image`, read for its metadata alone: each file an [`Ext4File`], its
    /// bytes left in the image. Symlinks' targets are read, being metadata.
    pub fn meta(&self, image: impl Read + Seek) -> Result<FsTree<Ext4File>> {
        walk(&mut self.0.with(image), |_, ino, inode| {
            Ok(Ext4File {
                ino,
                len: inode.size,
            })
        })
    }

    /// Up to `len` bytes of `file`'s contents from `offset`, read from `image`: fewer where they
    /// run past its end, none from past it. Refused if its inode is no longer a regular file.
    pub fn read(
        &self,
        image: impl Read + Seek,
        file: &Ext4File,
        offset: u64,
        len: u64,
    ) -> Result<Vec<u8>> {
        let mut reader = self.0.with(image);
        let inode = reader.inode(file.ino)?;
        if inode.mode & 0o170000 != 0o100000 {
            return Err(Error::Malformed(format!(
                "ext4 image: inode {} is mode {:o}, which is not a file",
                file.ino, inode.mode
            )));
        }
        reader.range(file.ino, &inode, offset, len)
    }
}

impl FsTree {
    /// The tree in `image`, an ext4 image from its byte zero, file contents and all.
    ///
    /// Read as the kernel's next mount would find it: what a guest killed midway left
    /// committed in the journal is replayed over the image -- in what is read, and not in
    /// the image -- and what it had not committed is not there. The image only has to be
    /// one no machine is running over.
    ///
    /// Refused if it has no journal and was not unmounted, since nothing then says what a
    /// guest left half done, and if its kernel found it inconsistent.
    pub fn from_ext4(image: impl Read + Seek) -> Result<FsTree> {
        walk(&mut Reader::open(image)?, |reader, ino, inode| {
            reader.contents(ino, inode)
        })
    }

    /// Write the tree onto `out` as a new ext4 image of `size` bytes, from its byte zero.
    ///
    /// `size` bounds what the guest may write and not what the image costs: nothing is
    /// written where the filesystem is empty, so a file `out` is stays as sparse as the
    /// host keeps it. For the same reason `out` has to read as zeros wherever nothing is
    /// written -- a new file, or an empty buffer.
    pub fn to_ext4(&self, out: impl Write + Seek, size: u64) -> Result<()> {
        let plan = Plan::new(self, size)?;
        let mut buffered = BufWriter::new(out);
        plan.emit(&mut buffered)
            .map_err(|e| Error::io("writing an ext4 image", e))?;
        buffered
            .into_inner()
            .map_err(|e| Error::io("writing an ext4 image", e.into_error()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::io::SeekFrom;
    use std::path::PathBuf;

    use super::*;
    use crate::fstree::{Entry, Meta};

    const BLOCK: u64 = 4096;

    /// A file in the temp directory, removed when this drops.
    struct Image(PathBuf);

    impl Image {
        fn write(name: &str, tree: &FsTree, size: u64) -> Image {
            let path = std::env::temp_dir()
                .join(format!("virtx-uvm-ext4-{name}-{}.ext4", std::process::id()));
            let _ = std::fs::remove_file(&path);
            tree.to_ext4(File::create_new(&path).unwrap(), size)
                .unwrap();
            Image(path)
        }

        fn read(&self) -> Result<FsTree> {
            FsTree::from_ext4(File::open(&self.0).unwrap())
        }

        fn block(&self, n: u64) -> Vec<u8> {
            let mut file = File::open(&self.0).unwrap();
            let mut block = vec![0u8; BLOCK as usize];
            file.seek(SeekFrom::Start(n * BLOCK)).unwrap();
            file.read_exact(&mut block).unwrap();
            block
        }

        fn put(&self, offset: u64, bytes: &[u8]) {
            let mut file = File::options().write(true).open(&self.0).unwrap();
            file.seek(SeekFrom::Start(offset)).unwrap();
            file.write_all(bytes).unwrap();
        }

        /// The block holding `data`, which has to be one block, and the only one like it.
        fn find(&self, data: &[u8]) -> u64 {
            let mut file = File::open(&self.0).unwrap();
            let blocks = file.seek(SeekFrom::End(0)).unwrap() / BLOCK;
            (0..blocks).find(|n| self.block(*n) == data).unwrap()
        }
    }

    impl Drop for Image {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn file(data: Vec<u8>) -> Entry {
        Entry::File {
            meta: Meta {
                mode: 0o644,
                ..Meta::default()
            },
            data,
        }
    }

    fn dir(mode: u16) -> Entry {
        Entry::Directory {
            meta: Meta {
                mode,
                ..Meta::default()
            },
            opaque: false,
        }
    }

    fn data(tree: &FsTree, path: &str) -> Vec<u8> {
        tree.walk()
            .into_iter()
            .find_map(|(at, _, entry)| match entry {
                Entry::File { data, .. } if at == path.as_bytes() => Some(data.clone()),
                _ => None,
            })
            .unwrap()
    }

    #[test]
    fn a_tree_reads_back_as_it_was_written() {
        let mut tree = FsTree::new();
        // The writer's own, made the same, so the trees can be compared whole.
        tree.insert(b"lost+found", dir(0o700)).unwrap();
        tree.insert(b"etc", dir(0o755)).unwrap();
        tree.insert(b"etc/hostname", file(b"guest\n".to_vec()))
            .unwrap();
        tree.insert(b"big", file(vec![7; 3 * BLOCK as usize + 5]))
            .unwrap();
        // Over a block of entries.
        tree.insert(b"many", dir(0o755)).unwrap();
        for i in 0..500 {
            let path = format!("many/file-with-a-longish-name-{i}");
            tree.insert(path.as_bytes(), file(i.to_string().into_bytes()))
                .unwrap();
        }

        let image = Image::write("roundtrip", &tree, 64 << 20);
        assert_eq!(image.read().unwrap().digest(), tree.digest());
    }

    /// Read for its metadata alone, an image is the tree it is read whole as, each file its
    /// inode and length; and each file's bytes are read from there, in any part.
    #[test]
    fn metadata_alone_is_the_tree_and_each_file_is_read_from_its_inode() {
        let mut tree = FsTree::new();
        tree.insert(b"lost+found", dir(0o700)).unwrap();
        tree.insert(b"etc", dir(0o755)).unwrap();
        tree.insert(b"etc/hostname", file(b"guest\n".to_vec()))
            .unwrap();
        tree.insert(b"empty", file(Vec::new())).unwrap();
        let big: Vec<u8> = (0..5 * BLOCK as usize + 123)
            .map(|i| (i % 251) as u8)
            .collect();
        tree.insert(b"big", file(big.clone())).unwrap();
        tree.link(b"etc/also-big", b"big").unwrap();
        let link = Entry::Symlink {
            meta: Meta::default(),
            target: b"etc/hostname".to_vec(),
        };
        tree.insert(b"link", link).unwrap();
        tree.insert(b"gone", Entry::Whiteout).unwrap();
        let opaque = Entry::Directory {
            meta: Meta {
                mode: 0o755,
                ..Meta::default()
            },
            opaque: true,
        };
        tree.insert(b"hidden", opaque).unwrap();
        let image = Image::write("meta", &tree, 64 << 20);

        let ext4 = Ext4::open(File::open(&image.0).unwrap()).unwrap();
        let meta = ext4.meta(File::open(&image.0).unwrap()).unwrap();
        let whole = image.read().unwrap();
        let lens = |tree: &FsTree<Vec<u8>>| tree.map(|data| data.len() as u64);
        assert_eq!(
            meta.map(|file| file.len).walk(),
            lens(&whole).walk(),
            "the same paths, entries and metadata"
        );
        assert!(matches!(
            meta.entry(b"link").unwrap(),
            Some(Entry::Symlink { target, .. }) if target == b"etc/hostname"
        ));
        assert_eq!(meta.entry(b"gone").unwrap(), Some(&Entry::Whiteout));
        assert!(matches!(
            meta.entry(b"hidden").unwrap(),
            Some(Entry::Directory { opaque: true, .. })
        ));

        let at = |path: &[u8]| match meta.entry(path).unwrap() {
            Some(Entry::File { data, .. }) => *data,
            other => panic!("{other:?}"),
        };
        // Two names of one file are one inode.
        assert_eq!(at(b"big"), at(b"etc/also-big"));
        let read = |path: &[u8], offset: u64, len: u64| {
            ext4.read(File::open(&image.0).unwrap(), &at(path), offset, len)
                .unwrap()
        };
        assert_eq!(read(b"etc/hostname", 0, 1 << 20), b"guest\n");
        assert_eq!(read(b"big", 0, u64::MAX), big);
        let (from, to) = (BLOCK as usize - 7, 3 * BLOCK as usize + 9);
        assert_eq!(read(b"big", from as u64, (to - from) as u64), big[from..to]);
        assert_eq!(read(b"big", big.len() as u64 - 3, 10), big[big.len() - 3..]);
        assert!(read(b"big", big.len() as u64, 10).is_empty());
        assert!(read(b"empty", 0, 10).is_empty());
        // An inode that is not a file is refused.
        let root = Ext4File { ino: 2, len: 0 };
        assert!(
            ext4.read(File::open(&image.0).unwrap(), &root, 0, 1)
                .is_err()
        );
    }

    #[test]
    fn what_the_journal_committed_is_read_and_nothing_else() {
        let block = |byte: u8| vec![byte; BLOCK as usize];
        let mut tree = FsTree::new();
        for (name, byte) in [(&b"a"[..], 1), (b"b", 2), (b"c", 3)] {
            tree.insert(name, file(block(byte))).unwrap();
        }
        let image = Image::write("journal", &tree, 64 << 20);
        let (a, b, c) = (
            image.find(&block(1)),
            image.find(&block(2)),
            image.find(&block(3)),
        );
        // The journal is one extent, whose first block `s_jnl_blocks` says.
        let sb = image.block(0);
        let journal = u64::from(u32::from_le_bytes(
            sb[1024 + 0x10C + 20..1024 + 0x10C + 24].try_into().unwrap(),
        ));

        const MAGIC: u32 = 0xC03B_3998;
        let header = |kind: u32, sequence: u32| {
            let mut b = vec![0u8; BLOCK as usize];
            b[0..4].copy_from_slice(&MAGIC.to_be_bytes());
            b[4..8].copy_from_slice(&kind.to_be_bytes());
            b[8..12].copy_from_slice(&sequence.to_be_bytes());
            b
        };
        // Tags of 8 bytes, each saying it has no UUID of its own.
        let descriptor = |sequence: u32, tags: &[(u64, u16)]| {
            let mut b = header(1, sequence);
            for (i, (target, flags)) in tags.iter().enumerate() {
                let at = 12 + 8 * i;
                let last = if i + 1 == tags.len() { 0x8 } else { 0 };
                b[at..at + 4].copy_from_slice(&(*target as u32).to_be_bytes());
                b[at + 6..at + 8].copy_from_slice(&(flags | 0x2 | last).to_be_bytes());
            }
            b
        };
        let commit = |sequence: u32| header(2, sequence);
        let revoke = |sequence: u32, target: u64| {
            let mut b = header(5, sequence);
            b[12..16].copy_from_slice(&20u32.to_be_bytes());
            b[16..20].copy_from_slice(&(target as u32).to_be_bytes());
            b
        };
        // What `c` becomes starts with the magic, which the log holds as zeros.
        let mut c_new = block(5);
        c_new[..4].copy_from_slice(&MAGIC.to_be_bytes());
        let mut c_logged = c_new.clone();
        c_logged[..4].fill(0);

        let log = [
            // 1: `a` and `c` rewritten.
            descriptor(1, &[(a, 0), (c, 0x1)]),
            block(4),
            c_logged,
            commit(1),
            // 2: `b` rewritten, and 3: that revoked.
            descriptor(2, &[(b, 0)]),
            block(6),
            commit(2),
            revoke(3, b),
            commit(3),
            // 4: `a` rewritten, and never committed.
            descriptor(4, &[(a, 0)]),
            block(7),
        ];
        for (n, logged) in log.iter().enumerate() {
            image.put((journal + 1 + n as u64) * BLOCK, logged);
        }
        image.put(journal * BLOCK + 0x1C, &1u32.to_be_bytes()); // s_start
        let incompat = u32::from_le_bytes(sb[1024 + 0x60..1024 + 0x64].try_into().unwrap());
        image.put(1024 + 0x60, &(incompat | 0x4).to_le_bytes()); // needs recovery

        let read = image.read().unwrap();
        assert_eq!(data(&read, "a"), block(4));
        assert_eq!(data(&read, "b"), block(2));
        assert_eq!(data(&read, "c"), c_new);

        // Read in parts, the same.
        let ext4 = Ext4::open(File::open(&image.0).unwrap()).unwrap();
        let meta = ext4.meta(File::open(&image.0).unwrap()).unwrap();
        for (name, whole) in [("a", block(4)), ("b", block(2)), ("c", c_new)] {
            let Some(Entry::File { data, .. }) = meta.entry(name.as_bytes()).unwrap() else {
                panic!("{name}");
            };
            let read = ext4.read(File::open(&image.0).unwrap(), data, 0, BLOCK);
            assert_eq!(read.unwrap(), whole, "{name}");
        }
    }

    #[test]
    fn an_image_without_a_journal_left_mounted_is_refused() {
        let image = Image::write("unclean", &FsTree::new(), 64 << 20);
        let sb = image.block(0);
        let compat = u32::from_le_bytes(sb[1024 + 0x5C..1024 + 0x60].try_into().unwrap());
        image.put(1024 + 0x5C, &(compat & !0x4).to_le_bytes());
        image.put(1024 + 0x3A, &0u16.to_le_bytes()); // s_state, as a mount leaves it
        assert!(matches!(image.read(), Err(Error::Malformed(_))));
    }

    #[test]
    fn an_image_of_more_blocks_than_32_bits_count_is_refused() {
        let size = (u64::from(u32::MAX) + 1) * BLOCK;
        let written = FsTree::new().to_ext4(std::io::Cursor::new(Vec::new()), size);
        assert!(matches!(written, Err(Error::Unwritable(_))));
    }
}
