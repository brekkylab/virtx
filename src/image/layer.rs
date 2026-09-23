//! Layer blobs, and turning one into something a guest can mount.
//!
//! A *layer blob* is what a registry serves: a tar archive of one filesystem
//! changeset, usually gzipped, named in a manifest by the digest of those compressed
//! bytes. The digest of the same archive uncompressed is its `diff_id`, and that is
//! what the image config signs and what names a layer's contents whatever a registry
//! happened to compress it with.
//!
//! The conversion here reads such an archive and writes an EROFS image. It reads the
//! archive twice -- once for the shape of the filesystem, once for the bytes -- which
//! is why it wants the uncompressed form, seekable. [`decompress`] is what turns a
//! blob into one, and learns the `diff_id` on the way past.
//!
//! Nothing in this module creates, renames or deletes a file. Whether the blob is
//! kept once its image exists is a question about the cache, and it is asked there.

use std::io::{Read, Seek, Write};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::image::erofs::{self, Layout};
use crate::image::{Digest, Error, Result};

/// One layer of an image.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Layer {
    /// Digest of the layer uncompressed, called `diff_id` in the OCI config. Names what
    /// the layer contains, whatever a registry happened to compress it with.
    pub digest: Digest,
}

/// Write the layer blob `blob` onto `out` uncompressed, returning its `diff_id`.
///
/// An OCI registry usually serves a layer compressed and says which compression in
/// the descriptor's `media_type`. The two spellings in the wild are OCI's
/// `...tar+gzip` and Docker's `...tar.gzip`, which is why this matches on a part and
/// not the whole.
///
/// The digest is of what was written, so a caller that has the layer's `diff_id` from
/// the image config can compare and learn whether the registry served what the config
/// signed. That check is the reason this returns a digest rather than the caller
/// hashing the archive afterwards: the bytes go past once either way.
pub fn decompress_blob(media_type: &str, mut blob: impl Read, out: impl Write) -> Result<Digest> {
    let mut sink = Hashing::new(out);

    let copied = if media_type.contains("zstd") {
        return Err(Error::Archive(format!(
            "{media_type} is not a layer this can read"
        )));
    } else if media_type.contains("gzip") {
        std::io::copy(&mut flate2::read::GzDecoder::new(blob), &mut sink)
    } else {
        std::io::copy(&mut blob, &mut sink)
    };
    copied.map_err(|e| Error::io("decompressing a layer blob", e))?;

    Ok(sink.digest())
}

/// Read the uncompressed layer archive `tar` and write it to `out` as an EROFS image.
///
/// `tar` is read from its start and read twice, so it has to be the whole archive and
/// it has to be seekable. `out` is written from its own byte zero.
pub fn blob_to_erofs<D, W>(tar: &mut D, out: &mut W) -> Result<Layout>
where
    D: Read + Seek,
    W: Write + Seek,
{
    tar.rewind()
        .map_err(|e| Error::io("rewinding a layer archive", e))?;
    let tree = crate::image::blob::ingest(&mut *tar)?;

    erofs::write(&tree, tar, out).map_err(Error::Erofs)
}

/// A writer that hashes what goes through it, so decompressing a blob and learning
/// its `diff_id` are one pass and not two.
struct Hashing<W> {
    inner: W,
    hasher: Sha256,
}

impl<W> Hashing<W> {
    fn new(inner: W) -> Self {
        Hashing {
            inner,
            hasher: Sha256::new(),
        }
    }

    fn digest(self) -> Digest {
        Digest::from_sha256(self.hasher.finalize().into())
    }
}

impl<W: Write> Write for Hashing<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let written = self.inner.write(buf)?;
        self.hasher.update(&buf[..written]);
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}
