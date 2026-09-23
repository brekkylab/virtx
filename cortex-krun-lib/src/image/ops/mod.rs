//! What a caller actually asks for, spelled out in terms of a [`Registry`] and a
//! [`Cache`].
//!
//! [`Registry`](super::Registry) is the wire and [`Cache`](super::Cache) the disk;
//! neither knows about the other. The operations here are where the two meet.

mod commit;
mod list;
mod provision;
mod prune;
mod pull;
mod remove;
mod tag;

pub use commit::commit;
pub use list::{Listed, list};
pub use provision::provision;
pub use prune::{GRACE, Pruned, prune};
pub use pull::pull;
pub use remove::{Removed, remove};
pub use tag::{tag, untag};

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use crate::image::{Cache, Digest, Image, Reference};

/// The image `reference` names, by the name it was filed under or by the digest it is.
///
/// Two lookups, because an image has two kinds of name and only one of them is the one
/// it was stored under. The index is keyed by the canonical spelling, so `alpine:3.21`
/// finds what `docker.io/library/alpine:3.21` pulled; and failing that a manifest digest
/// names the image rather than the way somebody reached it, so `repo@sha256:...` finds
/// what arrived under a tag -- whatever repository it is spelled with, the digest being
/// the stronger of the two names.
///
/// Answers with the name it was found under as well as the image, because every caller
/// here needs it: one to file a second name beside it, one to take it away.
pub(crate) fn find<'a>(
    index: &'a BTreeMap<String, Image>,
    reference: &Reference,
) -> Option<(&'a String, &'a Image)> {
    let name = reference.to_string();
    index.get_key_value(&name).or_else(|| {
        let digest = Digest::parse(reference.digest()?).ok()?;
        index.iter().find(|(_, image)| image.manifest == digest)
    })
}

/// Every file the images in `index` still need: each one's layers, and the merged
/// metadata and disk descriptor of the stack it is.
///
/// Shared by the two operations that delete things, because they have to agree exactly.
/// A stack is the ordered layers, so two images made of the same ones share a disk -- and
/// an answer that missed that would delete a disk something else is still reading.
pub(crate) fn referenced(index: &BTreeMap<String, Image>, cache: &Cache) -> HashSet<PathBuf> {
    let mut keep = HashSet::new();
    for image in index.values() {
        for layer in &image.layers {
            keep.insert(cache.layer_path(&layer.digest));
        }
        let stack = crate::image::stack::digest(&image.layers);
        keep.insert(cache.fsmeta_path(&stack));
        keep.insert(cache.disk_path(&stack));
    }
    keep
}

/// Delete a file and say what it weighed, or nothing if it was not there.
///
/// A failure to delete is not a failure of the operation: by the time this runs the index
/// no longer names the file, so what is left behind is space and not a broken image.
/// Reporting it would make callers handle an outcome they cannot act on.
pub(crate) fn unlink(path: &Path) -> Option<u64> {
    let bytes = std::fs::metadata(path).ok()?.len();
    std::fs::remove_file(path).ok()?;
    Some(bytes)
}

/// A cache with things in it, which is what three of the modules here have to start from.
///
/// Duplicated by [`pull`] and [`provision`], which had theirs before this existed and
/// build what they need differently -- a layer with a shape their own assertions read.
/// What is here is the smaller thing the rest want: a real layer file under a real name,
/// so that sizes are sizes and a file that is deleted was a file.
#[cfg(test)]
pub(crate) mod tests {
    use std::io::Cursor;
    use std::path::PathBuf;

    use crate::image::tree::{Extent, Meta, Node, Tree};
    use crate::image::{Cache, Digest, Image, Layer, erofs};

    pub(crate) struct Scratch(PathBuf);

    impl Scratch {
        pub(crate) fn new(name: &str) -> Scratch {
            let path =
                std::env::temp_dir().join(format!("cortex-krun-ops-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Scratch(path)
        }

        pub(crate) fn cache(&self) -> Cache {
            Cache::at(self.0.join("cache"))
        }

        /// Write a layer into the cache holding `files`, and hand back what names it.
        pub(crate) fn layer(&self, cache: &Cache, files: &[(&[u8], &[u8])]) -> Layer {
            let mut tree = Tree::new();
            let mut data = Vec::new();

            for (path, contents) in files {
                let offset = data.len() as u64;
                data.extend_from_slice(contents);
                let id = tree.next_inode();
                tree.insert(
                    path,
                    Node::File {
                        meta: Meta {
                            mode: 0o644,
                            uid: 0,
                            gid: 0,
                            mtime: 0,
                            xattrs: Vec::new(),
                        },
                        id,
                        data: Extent {
                            offset,
                            len: contents.len() as u64,
                        },
                    },
                )
                .unwrap();
            }

            // By what is in it, so that two layers with the same files are one file in the
            // cache -- which is the property the sweeping in `remove` turns on.
            let digest = Digest::of(
                files
                    .iter()
                    .flat_map(|(path, contents)| [*path, *contents])
                    .collect::<Vec<_>>()
                    .concat(),
            );
            let mut staged = cache.staged_writer(cache.layer_path(&digest)).unwrap();
            erofs::write(&tree, &mut Cursor::new(data), &mut staged).unwrap();
            staged.commit().unwrap();

            Layer { digest }
        }

        /// File an image over `files` under `reference`, as a pull would have left it.
        pub(crate) fn pulled(&self, cache: &Cache, reference: &str, files: &[(&[u8], &[u8])]) -> Image {
            let image = Image {
                manifest: Digest::of(reference.as_bytes()),
                layers: vec![self.layer(cache, files)],
                config: Default::default(),
                platform: "linux/arm64".to_string(),
            };
            self.tagged(cache, reference, &image);
            image
        }

        /// The same image under another name, which is what a tag is here.
        pub(crate) fn tagged(&self, cache: &Cache, reference: &str, image: &Image) {
            let reference: crate::image::Reference = reference.parse().unwrap();
            let mut index = cache.image_index_mut().unwrap();
            index.insert(reference.to_string(), image.clone());
            index.commit().unwrap();
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}
