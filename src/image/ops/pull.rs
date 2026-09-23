//! Putting an image into the cache.

use std::io::Cursor;

use oci_client::Reference;

use crate::image::{
    Cache, Digest, Error, Image, Layer, Registry, Result, blob_to_erofs, decompress_blob,
};

/// Put `reference` and everything behind it into `cache`, so that a boot can proceed
/// offline. Only what is missing is downloaded, and a failed attempt keeps whatever
/// did arrive.
///
/// `None` for either side takes the default: a [`Registry`] that reads what anybody can
/// read, and the [`Cache`] at `CORTEX_IMAGE_CACHE_ROOT` or below the user's cache
/// directory. Name one to pull from a private repository, or to keep the image
/// somewhere of your own.
///
/// A layer is held whole in memory only for as long as it takes to uncompress it,
/// and one already in the cache is not asked for at all.
pub async fn pull(
    reference: Reference,
    registry: Option<Registry>,
    cache: Option<Cache>,
) -> Result<Image> {
    let registry = registry.unwrap_or_default();
    let cache = cache.unwrap_or_default();

    let (image_manifest, digest, config) = registry.get_metadata(&reference).await?;

    // The manifest names each layer's compressed bytes and the config names its
    // contents, and nothing but their position pairs the two. A disagreement here is an
    // image nothing downstream can read correctly, so it is caught before anything is
    // fetched.
    if config.diff_ids().len() != image_manifest.layers.len() {
        return Err(Error::Manifest(
            reference.to_string(),
            format!(
                "{} layers in the manifest and {} diff_ids in the config",
                image_manifest.layers.len(),
                config.diff_ids().len()
            ),
        ));
    }

    let layers = config
        .diff_ids()
        .iter()
        .map(|diff_id| {
            Ok(Layer {
                digest: Digest::parse(diff_id)?,
            })
        })
        .collect::<Result<Vec<Layer>>>()?;

    for (index, descriptor) in image_manifest.layers.iter().enumerate() {
        // What the config signs this layer as, and so what the cache files its image
        // under. Asked before anything is fetched, so a layer already converted costs
        // neither the download nor the conversion.
        let expected = &layers[index].digest;
        if cache.has_erofs(expected) {
            continue;
        }

        // Downlod blob (compressed blob).
        let mut blob_compressed = Vec::with_capacity(descriptor.size as usize);
        registry
            .pull_blob(&reference, descriptor, &mut blob_compressed)
            .await?;

        // The archive behind the blob, and the `diff_id` that names it. Both stay in
        // memory: the compressed bytes are dropped here and never reach the cache,
        // and what reads the archive next wants it seekable, which a `Cursor` over
        // these bytes is without a file.
        //
        // Off the runtime, because inflating a layer is CPU that every other task on
        // these threads would otherwise wait behind.
        let media_type = descriptor.media_type.clone();
        let (diff_id, blob) = tokio::task::spawn_blocking(move || {
            let mut blob = Vec::new();
            let diff_id = decompress_blob(&media_type, Cursor::new(blob_compressed), &mut blob)?;
            Ok::<_, Error>((diff_id, blob))
        })
        .await
        .map_err(|e| Error::io("uncompressing a layer", std::io::Error::other(e)))??;

        // The config's word against the registry's bytes, asked before the conversion
        // rather than after it: a layer that uncompresses to something the config does
        // not name is not this image's layer, and converting it first would be several
        // hundred megabytes of work to throw away.
        if diff_id != *expected {
            return Err(Error::Manifest(
                reference.to_string(),
                format!("a layer uncompresses to {diff_id}, and its config signs {expected}"),
            ));
        }

        // The layer as something a guest can mount. Written into `tmp/` under a
        // `Staged`, so a failure here leaves nothing behind a name anything looks up,
        // and off the runtime for the same reason the decompression was.
        //
        // `Cursor` because the conversion reads the archive twice -- once for the
        // shape of the filesystem, once for the bytes -- and so needs it seekable.
        let mut erofs = cache.staged_writer(cache.erofs_path(&diff_id))?;
        let erofs = tokio::task::spawn_blocking(move || {
            blob_to_erofs(&mut Cursor::new(blob), &mut erofs)?;
            Ok::<_, Error>(erofs)
        })
        .await
        .map_err(|e| Error::io("writing a layer image", std::io::Error::other(e)))??;

        erofs.commit()?;
    }

    // Last, so the memo never names an image whose layers did not all arrive. What it
    // holds is what a boot reads and nothing else: the registry is not asked again, and
    // neither the manifest nor the config is kept as the bytes it arrived as.
    let image = Image {
        manifest: digest,
        layers,
        config: config.state(),
        platform: config.platform(),
    };
    let mut index = cache.image_index_mut()?;
    index.insert(reference.to_string(), image.clone());
    index.commit()?;

    Ok(image)
}
