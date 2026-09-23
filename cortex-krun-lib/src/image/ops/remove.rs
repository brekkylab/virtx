//! Taking an image out of the cache, and reclaiming what nothing else was using.

use std::fmt::Display;
use std::path::PathBuf;

use super::{referenced, unlink};
use crate::image::{Cache, Error, Reference, Result, stack};

/// What a removal came to.
#[derive(Clone, Debug, Default)]
pub struct Removed {
    /// Every name the image had, all of which are gone. More than one when it had been
    /// tagged -- a name is a key and an image is what several of them can point at.
    pub untagged: Vec<String>,

    /// The files deleted: layers, and the metadata image and descriptor of any stack that
    /// nothing else is built on.
    pub files: Vec<PathBuf>,

    /// What those files occupied, in bytes. This is the honest number and
    /// [`Listed::bytes`](super::Listed::bytes) is not: a layer another image is still
    /// using is not deleted and is not counted here.
    pub bytes: u64,
}

/// Take away every name of the image `reference` points at, then delete what is left over.
///
/// `reference` is resolved by the name it was filed under or by the manifest digest it is,
/// and **every** name of that image goes -- not only the one that was used to find it. An
/// image is one thing, so removing it is one act; taking a single name away and leaving
/// the rest is [`untag`](super::untag), which is a different question and has its own
/// answer.
///
/// What is then deleted is only what *this* image was holding and nothing else is:
///
/// - its layers, except the ones another image still stacks
/// - the merged metadata and the disk descriptor of its stack, unless another image has
///   the same layers in the same order
///
/// Nothing else in the cache is examined, let alone removed. That is what makes this safe
/// to run while other things are happening: a layer half-written by a pull in flight was
/// never part of this image, so it is never a candidate.
///
/// The index is written before any file is deleted. The two are not one act, so a process
/// that dies between them leaves files nothing names -- which costs space and nothing
/// else. The other order would leave names with nothing behind them, which costs a boot.
///
/// ```no_run
/// # fn demo() -> Result<(), Box<dyn std::error::Error>> {
/// use cortex_krun::image::remove;
///
/// let gone = remove("alpine:3.21", None)?;
/// println!("untagged {:?}, reclaimed {} MiB", gone.untagged, gone.bytes >> 20);
/// # Ok(())
/// # }
/// ```
pub fn remove<E: Display>(
    reference: impl TryInto<Reference, Error = E>,
    cache: Option<Cache>,
) -> Result<Removed> {
    let cache = cache.unwrap_or_default();

    let reference = reference
        .try_into()
        .map_err(|e| Error::Reference(e.to_string()))?;

    let mut index = cache.image_index_mut()?;

    let image = super::find(&index, &reference)
        .map(|(_, image)| image.clone())
        .ok_or_else(|| Error::Unknown(reference.to_string()))?;

    // What this image was holding, before anything is taken out of the index -- the only
    // files this call will ever consider deleting.
    let mut candidates: Vec<PathBuf> = image
        .layers
        .iter()
        .map(|layer| cache.layer_path(&layer.digest))
        .collect();
    let mine = stack::digest(&image.layers);
    candidates.push(cache.fsmeta_path(&mine));
    candidates.push(cache.disk_path(&mine));

    // Every name of it, and not only the one that found it.
    let untagged: Vec<String> = index
        .iter()
        .filter(|(_, other)| other.manifest == image.manifest)
        .map(|(name, _)| name.clone())
        .collect();
    for name in &untagged {
        index.remove(name);
    }

    // What the cache still needs, read off the index as it will be once this is written.
    let keep = referenced(&index, &cache);

    // Written first: files nothing names cost space, and names with nothing behind them
    // cost a boot.
    index.commit()?;

    let mut removed = Removed {
        untagged,
        ..Default::default()
    };
    for path in candidates {
        if keep.contains(&path) {
            continue;
        }
        if let Some(bytes) = unlink(&path) {
            removed.bytes += bytes;
            removed.files.push(path);
        }
    }

    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::super::tests::Scratch;
    use super::*;
    use crate::image::{Image, list, provision, tag};

    /// The whole of it: every name goes, and so does what nothing else was using.
    #[test]
    fn removing_takes_every_name_and_what_was_under_them() {
        let scratch = Scratch::new("remove");
        let cache = scratch.cache();
        let pulled = scratch.pulled(&cache, "test/base:1", &[(b"etc/one", b"a")]);
        tag("test/base:1", "test/current:1", Some(cache.clone())).unwrap();
        provision("test/base:1", Some(cache.clone())).unwrap();

        let layer = cache.layer_path(&pulled.layers[0].digest);
        let stack = stack::digest(&pulled.layers);
        assert!(layer.is_file() && cache.has_disk(&stack));

        // Found by one name, and both of them go -- an image is one thing.
        let gone = remove("test/current:1", Some(cache.clone())).unwrap();
        assert_eq!(gone.untagged.len(), 2, "{:?}", gone.untagged);
        assert!(gone.bytes > 0, "nothing was reclaimed");

        assert!(list(Some(cache.clone())).unwrap().is_empty());
        assert!(!layer.exists(), "the layer is still here");
        assert!(!cache.fsmeta_path(&stack).exists(), "the metadata is still here");
        assert!(!cache.disk_path(&stack).exists(), "the descriptor is still here");
    }

    /// The case the reckoning is for: a base under something else is not deleted with it.
    #[test]
    fn a_layer_another_image_stacks_is_kept() {
        let scratch = Scratch::new("shared");
        let cache = scratch.cache();

        let base = scratch.pulled(&cache, "test/base:1", &[(b"etc/base", b"a")]);
        let over = {
            let mut layers = base.layers.clone();
            layers.push(scratch.layer(&cache, &[(b"etc/over", b"b")]));
            let image = Image {
                manifest: crate::image::Digest::of(b"over"),
                layers,
                config: Default::default(),
                platform: "linux/arm64".to_string(),
            };
            scratch.tagged(&cache, "test/over:1", &image);
            image
        };

        let shared = cache.layer_path(&base.layers[0].digest);
        let only = cache.layer_path(&over.layers[1].digest);

        let gone = remove("test/over:1", Some(cache.clone())).unwrap();
        assert!(shared.is_file(), "the base's layer went with the image over it");
        assert!(!only.exists(), "the image's own layer is still here");
        assert!(
            gone.files.iter().all(|path| path != &shared),
            "a kept file was reported as reclaimed: {:?}",
            gone.files
        );

        // And the base is still a thing that boots.
        provision("test/base:1", Some(cache.clone())).unwrap();
    }

    /// Two names for one image share one stack, so removing one must not take the disk
    /// the other still reads.
    #[test]
    fn a_stack_another_name_shares_is_kept() {
        let scratch = Scratch::new("stack");
        let cache = scratch.cache();
        let first = scratch.pulled(&cache, "test/first:1", &[(b"etc/one", b"a")]);

        // A different image -- its own manifest -- built on exactly the same layers.
        let twin = Image {
            manifest: crate::image::Digest::of(b"twin"),
            layers: first.layers.clone(),
            ..first.clone()
        };
        scratch.tagged(&cache, "test/twin:1", &twin);
        provision("test/first:1", Some(cache.clone())).unwrap();

        let stack = stack::digest(&first.layers);
        remove("test/first:1", Some(cache.clone())).unwrap();

        assert!(
            cache.has_disk(&stack),
            "the disk an image with the same layers still reads was deleted"
        );
        assert!(cache.has_layer(&first.layers[0].digest));
        provision("test/twin:1", Some(cache.clone())).unwrap();
    }

    #[test]
    fn an_image_nothing_holds_is_refused() {
        let scratch = Scratch::new("absent");
        let cache = scratch.cache();
        assert!(matches!(
            remove("test/absent:1", Some(cache)),
            Err(Error::Unknown(_))
        ));
    }
}
