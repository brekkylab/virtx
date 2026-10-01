//! Giving an image another name, and taking one away.
//!
//! A name here is a key in `images.json` and nothing more. Nothing under it is copied by
//! tagging and nothing under it is deleted by untagging: the layers are addressed by what
//! they hold, so every name of one image points at one set of files.
//!
//! Untagging is therefore *not* removing -- see [`remove`](super::remove), which is.
//! Taking the last name off an image leaves its layers where they are, reachable by
//! `repo@sha256:...` and by nothing else. That is a deliberate split; the module doc above
//! says why.

use std::fmt::Display;

use crate::image::{Cache, Error, Image, Reference, Result};

/// File the image `from` names under `to` as well, and answer with the image.
///
/// Both are anything a [`Reference`] can be read out of, and `from` is resolved the way
/// everything here resolves one: by the name it was filed under, or by the manifest digest
/// it is. So `tag("alpine@sha256:0a7e...", "base:current")` works on an image whose tag
/// somebody has since moved.
///
/// The name replaces whatever held it. A tag is a name somebody is free to move, and
/// refusing to move one would make this the only thing in the cache that cannot be
/// corrected -- the image it pointed at is still there, under its digest.
///
/// ```no_run
/// # fn demo() -> Result<(), Box<dyn std::error::Error>> {
/// use virtx_uvm::image::tag;
///
/// tag("alpine:3.21", "base:current", None)?;
/// # Ok(())
/// # }
/// ```
pub fn tag<E: Display, F: Display>(
    from: impl TryInto<Reference, Error = E>,
    to: impl TryInto<Reference, Error = F>,
    cache: Option<Cache>,
) -> Result<Image> {
    let cache = cache.unwrap_or_default();

    // Both before the lock is taken, so a name that is not a name costs nothing and
    // nothing is held while it is being found out.
    let from = from
        .try_into()
        .map_err(|e| Error::Reference(e.to_string()))?;
    let to = to.try_into().map_err(|e| Error::Reference(e.to_string()))?;

    // Under the lock for the whole of it: what is read is what is written back, and a
    // second tag landing in between would otherwise be lost.
    let mut index = cache.image_index_mut()?;

    let image = super::find(&index, &from)
        .map(|(_, image)| image.clone())
        .ok_or_else(|| Error::Unknown(from.to_string()))?;

    index.insert(to.to_string(), image.clone());
    index.commit()?;

    Ok(image)
}

/// Take the name `reference` away, leaving everything it pointed at.
///
/// Answers with the image that had the name, which is what a caller needs to say what it
/// just untagged -- and to notice that it was the last name, its
/// [`manifest`](Image::manifest) being how the image can still be reached.
///
/// **Only a name that was filed.** A digest is what an image *is* rather than something it
/// was called, so `untag("repo@sha256:...")` is refused: there is no key to take away, and
/// taking away the tag that happens to point at the same image would be removing a name
/// the caller did not name.
pub fn untag<E: Display>(
    reference: impl TryInto<Reference, Error = E>,
    cache: Option<Cache>,
) -> Result<Image> {
    let cache = cache.unwrap_or_default();

    let reference = reference
        .try_into()
        .map_err(|e| Error::Reference(e.to_string()))?;
    let name = reference.to_string();

    let mut index = cache.image_index_mut()?;
    let image = index
        .remove(&name)
        .ok_or_else(|| Error::Unknown(name.clone()))?;
    index.commit()?;

    Ok(image)
}

#[cfg(test)]
mod tests {
    use super::super::tests::Scratch;
    use super::*;
    use crate::image::{list, provision};

    /// A tag is a second name for one image, and neither name costs anything the other
    /// did not already.
    #[test]
    fn a_tag_is_another_name_for_one_image() {
        let scratch = Scratch::new("tag");
        let cache = scratch.cache();
        let pulled = scratch.pulled(&cache, "test/base:1", &[(b"etc/one", b"a")]);

        let tagged = tag("test/base:1", "test/current:1", Some(cache.clone())).unwrap();
        assert_eq!(tagged.manifest, pulled.manifest);

        let listed = list(Some(cache.clone())).unwrap();
        assert_eq!(listed.len(), 2, "{listed:?}");
        assert!(
            listed
                .iter()
                .all(|entry| entry.image.manifest == pulled.manifest)
        );

        // And the new name is a name a boot can be given.
        provision("test/current:1", Some(cache.clone())).unwrap();
    }

    /// The digest is the stronger name, so it finds an image whose tag has moved.
    #[test]
    fn an_image_is_tagged_by_the_digest_it_is() {
        let scratch = Scratch::new("bydigest");
        let cache = scratch.cache();
        let pulled = scratch.pulled(&cache, "test/base:1", &[(b"etc/one", b"a")]);

        tag(
            format!("test/base@{}", pulled.manifest),
            "test/pinned:1",
            Some(cache.clone()),
        )
        .unwrap();

        let listed = list(Some(cache.clone())).unwrap();
        assert!(
            listed
                .iter()
                .any(|entry| entry.reference.contains("pinned")),
            "{listed:?}"
        );
    }

    /// A tag is a name somebody is free to move, and moving one is what this is.
    #[test]
    fn a_tag_that_is_taken_is_moved() {
        let scratch = Scratch::new("move");
        let cache = scratch.cache();
        let first = scratch.pulled(&cache, "test/first:1", &[(b"etc/one", b"a")]);
        let second = scratch.pulled(&cache, "test/second:1", &[(b"etc/two", b"b")]);
        assert_ne!(first.manifest, second.manifest);

        tag("test/first:1", "test/current:1", Some(cache.clone())).unwrap();
        tag("test/second:1", "test/current:1", Some(cache.clone())).unwrap();

        let moved = list(Some(cache.clone()))
            .unwrap()
            .into_iter()
            .find(|entry| entry.reference.contains("current"))
            .expect("the tag went missing");
        assert_eq!(moved.image.manifest, second.manifest);
    }

    /// Untagging takes the name and stops. What it pointed at is still there, and still
    /// reachable by the one name an image cannot lose.
    #[test]
    fn untagging_takes_a_name_and_nothing_else() {
        let scratch = Scratch::new("untag");
        let cache = scratch.cache();
        let pulled = scratch.pulled(&cache, "test/base:1", &[(b"etc/one", b"a")]);
        tag("test/base:1", "test/current:1", Some(cache.clone())).unwrap();

        let untagged = untag("test/current:1", Some(cache.clone())).unwrap();
        assert_eq!(untagged.manifest, pulled.manifest);

        let listed = list(Some(cache.clone())).unwrap();
        assert_eq!(listed.len(), 1, "{listed:?}");
        assert!(
            cache.has_layer(&pulled.layers[0].digest),
            "untagging deleted a layer"
        );

        // Even the last name going leaves the image, which is what parts this from remove.
        untag("test/base:1", Some(cache.clone())).unwrap();
        assert!(list(Some(cache.clone())).unwrap().is_empty());
        assert!(
            cache.has_layer(&pulled.layers[0].digest),
            "the last untag deleted a layer"
        );
    }

    #[test]
    fn a_name_nothing_holds_is_refused() {
        let scratch = Scratch::new("missing");
        let cache = scratch.cache();
        scratch.pulled(&cache, "test/base:1", &[(b"etc/one", b"a")]);

        assert!(matches!(
            untag("test/absent:1", Some(cache.clone())),
            Err(Error::Unknown(_))
        ));
        assert!(matches!(
            tag("test/absent:1", "test/new:1", Some(cache.clone())),
            Err(Error::Unknown(_))
        ));
        assert!(matches!(
            tag("NOT A REFERENCE", "test/new:1", Some(cache.clone())),
            Err(Error::Reference(_))
        ));
    }
}
