//! EROFS, the read-only filesystem a layer is so a guest can mount it.
//!
//! Only the format: whiteouts and archives belong to the changeset. A tree's whiteouts
//! and opaque directories are written here in overlayfs's spelling, which is what the
//! guest reads them with.

mod format;
mod reader;
mod writer;

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::Arc;

use self::format::BLOCK_SIZE;
use self::reader::{Reader, read};
use self::writer::write;
use super::{Error, FsTree, Result, Span};

impl FsTree {
    /// The tree in `image`, an EROFS image from its byte zero, file contents and all.
    pub fn from_erofs(image: impl Read + Seek) -> Result<FsTree> {
        read(&mut Reader::open(image)?, &mut |reader, inode| {
            reader.contents(inode)
        })
    }

    /// Write the tree onto `out` as an EROFS image, from its byte zero.
    pub fn to_erofs(&self, out: impl Write + Seek) -> Result<()> {
        write(self, out)?;
        Ok(())
    }
}

impl FsTree<Span> {
    /// The tree in the EROFS image at `path` -- a layer, as [`FsTree::to_erofs`] writes one
    /// -- read for its metadata only: each file a [`Span`] of the image, the bytes left
    /// there.
    ///
    /// **The image has to be exactly as long as its superblock says**, and one that is not
    /// is refused: a tree like this is written in front of the image on one disk, and
    /// whatever follows the image there is addressed as if it did.
    pub fn from_erofs_meta(path: &Path) -> Result<FsTree<Span>> {
        let file =
            File::open(path).map_err(|e| Error::io(format!("opening {}", path.display()), e))?;
        let mut reader = Reader::open(BufReader::new(file))?;
        let len = reader
            .image
            .seek(SeekFrom::End(0))
            .map_err(|e| Error::io(format!("reading {}", path.display()), e))?;
        if len != reader.bytes {
            return Err(Error::Malformed(format!(
                "EROFS image: {} is {len} bytes and its superblock accounts for {}",
                path.file_name().unwrap_or(path.as_os_str()).display(),
                reader.bytes
            )));
        }

        let path: Arc<Path> = Arc::from(path);
        read(&mut reader, &mut |reader, inode| {
            // Asked here because nothing reads the bytes to ask it later: a file that runs
            // past the image would run into whatever follows it on a disk.
            let offset = u64::from(inode.u) * BLOCK_SIZE;
            if offset.saturating_add(inode.size) > reader.bytes {
                return Err(Error::Malformed(format!(
                    "EROFS image: {} bytes at {offset} run past the end of the image",
                    inode.size
                )));
            }
            Ok(Span {
                path: path.clone(),
                block: inode.u,
                len: inode.size,
            })
        })
    }

    /// Write the tree onto `out` as an EROFS image, from its byte zero, and answer with the
    /// images its files point into, in the order they have to follow it.
    ///
    /// A file is not written: its inode points past this image's own end, to where a disk
    /// made of this image and then each of those images, end to end, has its bytes. An
    /// image nothing points into any more -- every file of it replaced or deleted above --
    /// is not among them.
    pub fn to_erofs_meta(&self, out: impl Write + Seek) -> Result<Vec<Arc<Path>>> {
        write(self, out)
    }
}

#[cfg(test)]
mod tests {
    use std::fs::OpenOptions;
    use std::io::Cursor;

    use super::*;
    use crate::fstree::{Entry, Meta};

    #[test]
    fn an_image_of_the_wrong_length_is_named_by_its_file_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("layer.erofs");
        let mut tree = FsTree::new();
        let hostname = Entry::File {
            meta: Meta::default(),
            data: b"host".to_vec(),
        };
        tree.insert(b"etc/hostname", hostname).unwrap();
        tree.to_erofs(File::create(&path).unwrap()).unwrap();
        let spans = FsTree::from_erofs_meta(&path).unwrap();

        let mut image = OpenOptions::new().append(true).open(&path).unwrap();
        image.write_all(b"\0").unwrap();
        let refusals = [
            FsTree::from_erofs_meta(&path).unwrap_err(),
            spans.to_erofs_meta(Cursor::new(Vec::new())).unwrap_err(),
        ];
        let dir = dir.path().display().to_string();
        for e in refusals {
            let said = e.to_string();
            assert!(said.contains("EROFS image: layer.erofs is "), "{said}");
            assert!(!said.contains(&dir), "{said}");
        }
    }
}
