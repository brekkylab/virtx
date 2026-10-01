//! Putting what a session changed back into the cache, as an image over the one it ran on.
//!
//! The other end of [`pull`](super::pull). That one takes an image apart as a registry
//! serves it and files the pieces; this one takes a layer that was made here and files it
//! the same way, so that everything downstream -- [`provision`](super::provision) above
//! all -- cannot tell the two apart. A built image is an image: it is in `images.json`
//! under a name, its layers are in `layer/`, and a boot stitches it without knowing where
//! it came from.
//!
//! What a layer is by the time it reaches here is an uncompressed OCI layer archive --
//! what [`Ext4::to_blob`](crate::session::Ext4::to_blob) writes out of a session's
//! writable disk, and what a registry would have served compressed.

use std::fmt::Display;
use std::path::Path;

use crate::image::{
    Cache, Digest, Error, Image, ImageState, Layer, Reference, Result, blob_to_erofs,
};

/// How a built image's manifest digest is named, so that a change to what goes into it
/// cannot be read as an image somebody once built.
const MANIFEST: &str = "virtx-uvm/built-image/1\n";

/// File an image over `base` under `reference`, and answer with what was filed.
///
/// `layer` is the archive a build produced and the `diff_id` that names it -- the pair
/// [`to_blob`](crate::session::Ext4::to_blob) hands back. It is converted into the EROFS a
/// disk is stitched from and put in the cache under that digest, and a digest already
/// there is left alone: the name addresses the contents, so a layer that exists is the
/// same layer.
///
/// **`None` is a build that changed nothing about the filesystem.** A recipe of nothing
/// but `ENV` and `WORKDIR` changes what the image *states* and not what is in it, and an
/// empty layer over the base would be a layer that says so in several thousand bytes. The
/// image filed then has the base's layers and the caller's config.
///
/// `config` is what the image states about running -- the base's, with whatever the
/// recipe changed. It is taken whole rather than as a diff, because the caller is what
/// accumulated it and this has no way to apply a change it cannot see.
///
/// The image's own manifest digest is computed from what it is made of, there being no
/// registry to have signed one. Two builds that came to the same layer over the same base
/// with the same config are the same image and are named as one -- which is what makes a
/// second build of an unchanged recipe cost nothing.
///
/// ```no_run
/// # fn demo(base: &virtx_uvm::image::Image, blob: &std::path::Path,
/// #         diff_id: &virtx_uvm::image::Digest) -> Result<(), Box<dyn std::error::Error>> {
/// use virtx_uvm::image::commit;
///
/// let built = commit(base, Some((blob, diff_id)), base.config.clone(), "local/built:1", None)?;
/// # let _ = built;
/// # Ok(())
/// # }
/// ```
pub fn commit<E: Display>(
    base: &Image,
    layer: Option<(&Path, &Digest)>,
    config: ImageState,
    reference: impl TryInto<Reference, Error = E>,
    cache: Option<Cache>,
) -> Result<Image> {
    let cache = cache.unwrap_or_default();

    // Before anything is written, so a name that is not a name costs no conversion -- and
    // so that what lands in the index is the canonical spelling, as a pull's does.
    let reference = reference
        .try_into()
        .map_err(|e| Error::Reference(e.to_string()))?;

    let mut layers = base.layers.clone();
    if let Some((blob, diff_id)) = layer {
        put(blob, diff_id, &cache)?;
        layers.push(Layer {
            digest: diff_id.clone(),
        });
    }

    let image = Image {
        manifest: manifest(base, layer.map(|(_, diff_id)| diff_id), &config)?,
        layers,
        config,
        // The base's, and not this host's: what an image can run on is decided by the
        // layers, and every one of them but the new one came from there. A build that
        // ran somewhere else would have produced a different base.
        platform: base.platform.clone(),
    };

    // Last, so the index never names an image whose layer did not land.
    let mut index = cache.image_index_mut()?;
    index.insert(reference.to_string(), image.clone());
    index.commit()?;

    Ok(image)
}

/// Convert `blob` into the layer image a disk is stitched from, unless it is already here.
///
/// Written into `tmp/` under a [`Staged`](crate::image::Staged) and renamed, so a
/// conversion that failed part-way leaves nothing behind a name anything looks up.
fn put(blob: &Path, diff_id: &Digest, cache: &Cache) -> Result<()> {
    if cache.has_layer(diff_id) {
        return Ok(());
    }

    // The archive is read twice -- once for the shape of the filesystem, once for the
    // bytes -- so it is opened as a file and not taken as a stream.
    let mut archive = std::fs::File::open(blob)
        .map_err(|e| Error::io(format!("opening {}", blob.display()), e))?;

    let mut erofs = cache.staged_writer(cache.layer_path(diff_id))?;
    blob_to_erofs(&mut archive, &mut erofs)?;
    erofs.commit()
}

/// What names a built image, there being no registry to have named it.
///
/// The digest of what the image is made of, in a spelling of this repository's own: the
/// base it was built over, the layer that was put on it, and what it states about running.
/// Every image with those three the same *is* the same image, which is the property the
/// index needs -- a manifest digest is what [`provision`](super::provision) falls back to
/// when a `repo@sha256:...` is asked for.
fn manifest(base: &Image, diff_id: Option<&Digest>, config: &ImageState) -> Result<Digest> {
    let stated = serde_json::to_string(config)
        .map_err(|e| Error::io("naming a built image", std::io::Error::other(e)))?;

    let mut text = String::from(MANIFEST);
    text.push_str(&base.manifest.to_string());
    text.push('\n');
    // Spelled even when there is none, so that "nothing was added" and "something was
    // added" cannot come to the same text.
    match diff_id {
        Some(digest) => text.push_str(&digest.to_string()),
        None => text.push_str("-"),
    }
    text.push('\n');
    text.push_str(&stated);
    text.push('\n');

    Ok(Digest::of(text))
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::path::PathBuf;

    use super::*;
    use crate::image::provision;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Scratch {
            let path = std::env::temp_dir()
                .join(format!("virtx-uvm-commit-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Scratch(path)
        }

        fn cache(&self) -> Cache {
            Cache::at(self.0.join("cache"))
        }

        /// A layer archive holding `files`, as a path, which is how `commit` takes one.
        fn blob(&self, name: &str, files: &[(&str, &[u8])]) -> PathBuf {
            let mut archive = tar::Builder::new(Vec::new());
            for (path, contents) in files {
                let mut header = tar::Header::new_gnu();
                header.set_entry_type(tar::EntryType::Regular);
                header.set_mode(0o644);
                header.set_size(contents.len() as u64);
                archive
                    .append_data(&mut header, path, Cursor::new(*contents))
                    .unwrap();
            }
            let bytes = archive.into_inner().unwrap();

            let at = self.0.join(name);
            std::fs::write(&at, bytes).unwrap();
            at
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A base with one layer in the cache, as a pull would have left it.
    fn base(scratch: &Scratch, cache: &Cache) -> Image {
        let blob = scratch.blob("base.tar", &[("etc/base", b"from the base\n")]);
        let diff_id = Digest::of(std::fs::read(&blob).unwrap());
        put(&blob, &diff_id, cache).unwrap();

        let image = Image {
            manifest: Digest::of(b"a base manifest"),
            layers: vec![Layer { digest: diff_id }],
            config: ImageState {
                cmd: Some(vec!["/bin/sh".into()]),
                ..Default::default()
            },
            platform: "linux/arm64".to_string(),
        };

        let mut index = cache.image_index_mut().unwrap();
        index.insert("test/base:1".to_string(), image.clone());
        index.commit().unwrap();
        image
    }

    /// The whole of what commit is for: what a build wrote becomes an image nothing
    /// downstream can tell from a pulled one -- it is in the index, and it provisions.
    #[test]
    fn a_built_image_is_an_image() {
        let scratch = Scratch::new("built");
        let cache = scratch.cache();
        let base = base(&scratch, &cache);

        let blob = scratch.blob("built.tar", &[("etc/built", b"from the build\n")]);
        let diff_id = Digest::of(std::fs::read(&blob).unwrap());

        let built = commit(
            &base,
            Some((&blob, &diff_id)),
            base.config.clone(),
            "test/built:1",
            Some(cache.clone()),
        )
        .unwrap();

        assert_eq!(built.layers.len(), 2, "the layer was not put on the base");
        assert_eq!(built.layers[0].digest, base.layers[0].digest);
        assert_eq!(built.layers[1].digest, diff_id);
        assert_ne!(built.manifest, base.manifest, "it is the base's image");
        assert!(cache.has_layer(&diff_id), "the layer is not in the cache");

        // Filed under its name, and stitchable -- which is the only thing a boot asks of
        // an image and the whole reason this writes one.
        let index = cache.image_index().unwrap();
        assert!(index.contains_key("docker.io/test/built:1"), "{index:?}");
        provision("test/built:1", Some(cache.clone())).unwrap();
    }

    /// A recipe of nothing but `ENV` changes what the image says and not what is in it,
    /// and an empty layer saying so would be several thousand bytes of nothing.
    #[test]
    fn a_build_that_wrote_nothing_adds_no_layer() {
        let scratch = Scratch::new("stated");
        let cache = scratch.cache();
        let base = base(&scratch, &cache);

        let config = ImageState {
            env: Some(vec!["TZ=UTC".to_string()]),
            ..base.config.clone()
        };
        let built = commit(&base, None, config, "test/stated:1", Some(cache.clone())).unwrap();

        let digests = |image: &Image| -> Vec<Digest> {
            image
                .layers
                .iter()
                .map(|layer| layer.digest.clone())
                .collect()
        };
        assert_eq!(digests(&built), digests(&base), "a layer was invented");
        assert_eq!(
            built.config.env.as_deref(),
            Some(&["TZ=UTC".to_string()][..])
        );
        assert_ne!(
            built.manifest, base.manifest,
            "an image that states something else is the same image"
        );
    }

    /// The property a build cache rests on: the same layer over the same base stating the
    /// same thing is one image, whatever it was filed under.
    #[test]
    fn the_same_build_names_the_same_image() {
        let scratch = Scratch::new("again");
        let cache = scratch.cache();
        let base = base(&scratch, &cache);

        let blob = scratch.blob("again.tar", &[("etc/again", b"same\n")]);
        let diff_id = Digest::of(std::fs::read(&blob).unwrap());

        let once = commit(
            &base,
            Some((&blob, &diff_id)),
            base.config.clone(),
            "test/once:1",
            Some(cache.clone()),
        )
        .unwrap();
        let twice = commit(
            &base,
            Some((&blob, &diff_id)),
            base.config.clone(),
            "test/twice:1",
            Some(cache.clone()),
        )
        .unwrap();

        assert_eq!(once.manifest, twice.manifest);

        // And a different statement is a different image, or the name would be worthless.
        let stated = commit(
            &base,
            Some((&blob, &diff_id)),
            ImageState {
                working_dir: Some("/work".to_string()),
                ..base.config.clone()
            },
            "test/stated:1",
            Some(cache.clone()),
        )
        .unwrap();
        assert_ne!(once.manifest, stated.manifest);
    }
}
