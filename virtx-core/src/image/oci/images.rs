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
    // The catalog, a repository's tags and a tag's manifest are three requests, and another
    // client may remove a repository or a tag between them: what is gone is left out.
    for name in registry.repositories().await? {
        let Some(tags) = registry.tags(&name).await? else {
            continue;
        };
        for tag in tags {
            let Some(digest) = registry.manifest_digest_if_there(&name, &tag).await? else {
                continue;
            };
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
    let named = source;
    let source = Reference::parse(source)?;
    let (name, tag) = Reference::parse_tag(target)?;
    let registry = Registry::new(client);
    let manifest = held(
        named,
        registry.manifest(&source.name, source.target()).await,
    )?;
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
    held(
        reference,
        Registry::new(client).delete_manifest(&name, &tag).await,
    )
}

/// Remove the image `reference` names from its repository, and every tag there on it.
pub(crate) async fn remove(client: &TransportClient, reference: &str) -> Result<(), Error> {
    let named = reference;
    let reference = Reference::parse(reference)?;
    let registry = Registry::new(client);
    let digest = match &reference.target {
        Target::Digest(digest) => digest.clone(),
        Target::Tag(tag) => held(named, registry.manifest_digest(&reference.name, tag).await)?,
    };
    held(
        named,
        registry.delete_manifest(&reference.name, &digest).await,
    )
}

/// `result`, of a request for the image `reference` names, with its `404` told as an image the
/// server does not hold, as the CLI tells one, rather than as the registry request it was.
fn held<T>(reference: &str, result: Result<T, Error>) -> Result<T, Error> {
    result.map_err(|e| match e.status() {
        Some(404) => Error::Status(
            404,
            format!("not_found: {reference} is not an image the server holds"),
        ),
        _ => e,
    })
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use axum::{
        Json, Router,
        http::{StatusCode, header},
        response::IntoResponse,
        routing::{get, head},
    };
    use serde_json::json;

    use super::{list, remove, tag, untag};
    use crate::Error;
    use crate::transport::testing::piped;

    #[tokio::test]
    async fn a_list_leaves_out_what_was_removed_meanwhile() {
        let unknown = |code: &'static str| {
            (
                StatusCode::NOT_FOUND,
                Json(json!({"errors": [{"code": code, "message": "gone"}]})),
            )
        };
        let router = Router::new()
            .route(
                "/images/oci/v2/_catalog",
                get(|| async { Json(json!({"repositories": ["gone", "kept"]})) }),
            )
            .route(
                "/images/oci/v2/gone/tags/list",
                get(move || async move { unknown("NAME_UNKNOWN") }),
            )
            .route(
                "/images/oci/v2/kept/tags/list",
                get(|| async { Json(json!({"tags": ["latest", "lost"]})) }),
            )
            .route(
                "/images/oci/v2/kept/manifests/latest",
                head(|| async {
                    [
                        (
                            header::CONTENT_TYPE.as_str(),
                            "application/vnd.oci.image.manifest.v1+json",
                        ),
                        ("docker-content-digest", "sha256:ab12"),
                    ]
                    .into_response()
                }),
            )
            .route(
                "/images/oci/v2/kept/manifests/lost",
                head(move || async move { unknown("MANIFEST_UNKNOWN") }),
            );
        let client = piped(router);
        let listed = list(&client).await.unwrap();
        assert_eq!(
            listed
                .iter()
                .map(|image| (image.reference.as_str(), image.digest.as_str()))
                .collect::<Vec<_>>(),
            [("kept:latest", "sha256:ab12")]
        );
        client.close().await;
    }

    /// An image the server does not hold is refused `404`, `not_found`, by the reference it was
    /// asked for, by each call that takes one: what to branch on, as for a sandbox or a volume.
    #[tokio::test]
    async fn an_image_the_server_does_not_hold_is_refused_404_by_its_reference() {
        let unknown = || async {
            (
                StatusCode::NOT_FOUND,
                Json(json!({"errors": [{"code": "MANIFEST_UNKNOWN", "message": "unknown"}]})),
            )
        };
        let client = piped(Router::new().fallback(unknown));
        let digest = format!("nope@sha256:{}", "0".repeat(64));
        for (call, reference, result) in [
            ("untag", "nope:1", untag(&client, "nope:1").await),
            ("remove", "nope:1", remove(&client, "nope:1").await),
            ("remove", digest.as_str(), remove(&client, &digest).await),
            ("tag", "nope:1", tag(&client, "nope:1", "other:1").await),
        ] {
            let refused = result.unwrap_err();
            assert_eq!(refused.status(), Some(404), "{call} {reference}: {refused}");
            assert_eq!(
                refused.to_string(),
                format!(
                    "the server answered 404 Not Found: not_found: {reference} is not an image \
                     the server holds"
                ),
                "{call}"
            );
        }
        client.close().await;
    }

    /// A registry failure other than a `404` still names the request it answered, with no
    /// status to branch on.
    #[tokio::test]
    async fn a_registry_failure_other_than_404_names_its_request() {
        let failing = || async { (StatusCode::INTERNAL_SERVER_ERROR, "") };
        let client = piped(Router::new().fallback(failing));
        let refused = untag(&client, "app:1").await.unwrap_err();
        assert!(matches!(refused, Error::Registry(_)), "{refused:?}");
        assert_eq!(refused.status(), None);
        assert!(
            refused
                .to_string()
                .starts_with("DELETE app/manifests/1: 500 Internal Server Error"),
            "{refused}"
        );
        client.close().await;
    }
}
