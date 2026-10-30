//! Listing, tagging and removing images in the server's registry, as `PROTOCOL.md`'s Images
//! says each is done.

use super::{
    api::Registry,
    manifest::{Child, Manifest},
    reference::{Reference, Target},
};
use crate::{
    Error,
    image::{ImageListArgs, ImageListOutputItem},
    transport::TransportClient,
};

/// The tags `page` asks for, by repository and then tag, with the digest each points at: the
/// registry asked for no more than that, and only those tags' digests.
pub(crate) async fn list(
    client: &TransportClient,
    page: ImageListArgs,
) -> Result<Vec<ImageListOutputItem>, Error> {
    let registry = Registry::new(client);
    let limit = page.limit.map(|limit| limit as usize);
    let mut tags: Vec<(String, String)> = Vec::new();
    // The rest of `after`'s repository first, then the repositories after it.
    let repositories = match &page.after {
        Some(after) => {
            let (name, tag) = after.rsplit_once(':').ok_or_else(|| {
                Error::Registry(format!("after: {after:?} is not a reference, name:tag"))
            })?;
            for later in registry.tags_after(name, Some(tag), limit).await? {
                tags.push((name.to_owned(), later));
            }
            registry.repositories_after(name).await?
        }
        None => registry.repositories().await?,
    };
    for name in repositories {
        let left = limit.map(|limit| limit - tags.len());
        if left == Some(0) {
            break;
        }
        for tag in registry.tags_after(&name, None, left).await? {
            tags.push((name.clone(), tag));
        }
    }
    let mut images = Vec::new();
    for (name, tag) in tags {
        let digest = registry.manifest_digest(&name, &tag).await?;
        images.push(ImageListOutputItem {
            reference: format!("{name}:{tag}"),
            digest,
        });
    }
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
