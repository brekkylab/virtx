//! Listing, tagging and removing images in the server's registry, as `PROTOCOL.md`'s Images
//! says each is done.

use super::{
    api::Registry,
    manifest::{Child, Manifest},
    reference::{Reference, Target},
};
use crate::{Error, image::ImageListOutputItem, transport::TransportClient};

/// Every tag in every repository, with the digest it points at, sorted by reference.
pub(crate) async fn list(client: &TransportClient) -> Result<Vec<ImageListOutputItem>, Error> {
    let registry = Registry::new(client);
    let mut images = Vec::new();
    for name in registry.repositories().await? {
        for tag in registry.tags(&name).await? {
            let digest = registry.manifest_digest(&name, &tag).await?;
            images.push(ImageListOutputItem {
                reference: format!("{name}:{tag}"),
                digest,
            });
        }
    }
    images.sort_by(|a, b| a.reference.cmp(&b.reference));
    Ok(images)
}

/// Point the tag `target` at the image `source` names. Into another repository, everything
/// the image names is mounted there from `source`'s first.
pub(crate) async fn tag(client: &TransportClient, source: &str, target: &str) -> Result<(), Error> {
    let source = Reference::parse(source)?;
    let (name, tag) = Reference::parse_tag(target)?;
    let registry = Registry::new(client);
    let manifest = registry.manifest(&source.name, source.target()).await?;
    if name != source.name {
        copy(&registry, &source.name, &name, &manifest).await?;
    }
    registry.put_manifest(&name, &tag, &manifest).await
}

/// Give `to` everything `manifest`, in `from`, names: each manifest by its digest, after what
/// it names in turn, and each blob.
async fn copy(
    registry: &Registry<'_>,
    from: &str,
    to: &str,
    manifest: &Manifest,
) -> Result<(), Error> {
    for child in manifest.children()? {
        match child {
            Child::Manifest(descriptor) => {
                let child = registry.manifest(from, &descriptor.digest).await?;
                Box::pin(copy(registry, from, to, &child)).await?;
                registry.put_manifest(to, &child.digest, &child).await?;
            }
            Child::Blob(descriptor) => {
                let digest = &descriptor.digest;
                if registry.has_blob(to, digest).await? {
                    continue;
                }
                if let Some(upload) = registry.mount_blob(to, digest, from).await? {
                    let blob = registry.blob(from, digest).await?;
                    registry
                        .finish_upload(upload, digest, descriptor.size, blob)
                        .await?;
                }
            }
        }
    }
    Ok(())
}

/// Remove the tag `reference`, leaving the image it points at.
pub(crate) async fn untag(client: &TransportClient, reference: &str) -> Result<(), Error> {
    let (name, tag) = Reference::parse_tag(reference)?;
    Registry::new(client).delete_manifest(&name, &tag).await
}

/// Remove the image `reference` names from its repository, and every tag there on it.
pub(crate) async fn remove(client: &TransportClient, reference: &str) -> Result<(), Error> {
    let reference = Reference::parse(reference)?;
    let registry = Registry::new(client);
    let digest = match &reference.target {
        Target::Digest(digest) => digest.clone(),
        Target::Tag(tag) => registry.manifest_digest(&reference.name, tag).await?,
    };
    registry.delete_manifest(&reference.name, &digest).await
}
