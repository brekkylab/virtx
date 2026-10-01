//! What is in the cache.

use std::path::Path;

use crate::image::{Cache, Image, Result, stack};

/// One entry of the cache: a name, the image it points at, and what it costs.
#[derive(Clone, Debug)]
pub struct Listed {
    /// The name as the index holds it, which is the canonical spelling -- so
    /// `alpine:3.21` appears here as `docker.io/library/alpine:3.21`.
    pub reference: String,

    pub image: Image,

    /// What this image's layers occupy, in bytes.
    ///
    /// **Shared layers are counted in every image that has them**, so these do not add up
    /// to what the cache occupies: two images over one base each report the base. What
    /// the number answers is "what would this cost on its own", which is the question a
    /// caller deciding what to remove is asking -- and [`remove`](super::remove) answers
    /// the other one, by saying what it actually reclaimed.
    ///
    /// A layer whose file is missing counts as nothing rather than failing the listing:
    /// what is being described is a cache, and a cache with a hole in it is a fact about
    /// it worth being able to see.
    pub bytes: u64,

    /// Whether the disk a boot attaches has already been stitched. A `false` here is a
    /// first boot that will pay for one -- nothing is wrong with it.
    pub provisioned: bool,
}

/// Every image in `cache`, by name.
///
/// In the index's order, which is the canonical spelling sorted -- so the listing is
/// stable between calls and every spelling of one image sits under one entry.
///
/// One image with two names is two entries, because two names is what it has. They are
/// recognisable as one image by their [`manifest`](Image::manifest), which is what
/// [`remove`](super::remove) uses to take all of them at once.
///
/// ```no_run
/// # fn demo() -> Result<(), Box<dyn std::error::Error>> {
/// use virtx_uvm::image::list;
///
/// for entry in list(None)? {
///     println!("{} {} {} MiB", entry.reference, entry.image.platform, entry.bytes >> 20);
/// }
/// # Ok(())
/// # }
/// ```
pub fn list(cache: Option<Cache>) -> Result<Vec<Listed>> {
    let cache = cache.unwrap_or_default();

    Ok(cache
        .image_index()?
        .into_iter()
        .map(|(reference, image)| {
            let bytes = image
                .layers
                .iter()
                .map(|layer| size(&cache.layer_path(&layer.digest)))
                .sum();

            Listed {
                reference,
                provisioned: cache.has_disk(&stack::digest(&image.layers)),
                bytes,
                image,
            }
        })
        .collect())
}

/// How large a file is, and nothing for one that is not there.
fn size(path: &Path) -> u64 {
    std::fs::metadata(path).map(|at| at.len()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::ops::tests::Scratch;

    #[test]
    fn a_listing_says_what_is_there_and_what_it_costs() {
        let scratch = Scratch::new("list");
        let cache = scratch.cache();
        assert!(list(Some(cache.clone())).unwrap().is_empty(), "a new cache");

        let base = scratch.pulled(&cache, "test/base:1", &[(b"etc/one", b"a")]);
        scratch.tagged(&cache, "test/again:1", &base);

        let listed = list(Some(cache.clone())).unwrap();
        assert_eq!(listed.len(), 2, "two names is two entries: {listed:?}");

        // One image under both, which is what makes them one thing to remove.
        assert_eq!(listed[0].image.manifest, listed[1].image.manifest);
        assert!(listed[0].bytes > 0, "the layer weighs nothing");
        assert!(!listed[0].provisioned, "nothing has been stitched yet");

        crate::image::provision("test/base:1", Some(cache.clone())).unwrap();
        let listed = list(Some(cache.clone())).unwrap();
        assert!(
            listed.iter().all(|entry| entry.provisioned),
            "one stitch serves every name of one image: {listed:?}"
        );
    }
}
