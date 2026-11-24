//! Listing, tagging and removing images in the server's registry, as the protocol's Images says
//! each is done.

use super::{
    api::Registry,
    manifest::{Child, Manifest},
    reference::{Reference, Target},
};
use futures_util::{StreamExt as _, TryStreamExt as _, stream};

use crate::{Error, Members, image::ImageListOutputItem, transport::TransportClient};

/// How many of a listing's requests are in flight at once: one a tag, which over a remote
/// server would otherwise each wait out a round trip in turn.
const AT_ONCE: usize = 16;

/// Every tag in every repository, with the digest it points at, sorted by reference.
pub(crate) async fn list(client: &TransportClient) -> Result<Vec<ImageListOutputItem>, Error> {
    let registry = &Registry::new(client);
    // The catalog, a repository's tags and a tag's manifest are three requests, and another
    // client may remove a repository or a tag between them: what is gone is left out.
    let tagged: Vec<Vec<(String, String)>> = stream::iter(registry.repositories().await?)
        .map(|name| async move {
            let tags = registry.tags(&name).await?.unwrap_or_default();
            Ok::<_, Error>(tags.into_iter().map(|tag| (name.clone(), tag)).collect())
        })
        .buffer_unordered(AT_ONCE)
        .try_collect()
        .await?;
    let mut images: Vec<ImageListOutputItem> = stream::iter(tagged.into_iter().flatten())
        .map(|(name, tag)| async move {
            let digest = registry.manifest_digest_if_there(&name, &tag).await?;
            Ok::<_, Error>(digest.map(|digest| ImageListOutputItem {
                reference: format!("{name}:{tag}"),
                digest,
            }))
        })
        .buffer_unordered(AT_ONCE)
        .try_filter_map(|image| async move { Ok(image) })
        .try_collect()
        .await?;
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
            Members::default(),
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

    use std::sync::{Arc, Mutex};

    use axum::extract::Request;

    use super::{list, remove, tag, untag};
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

    /// A listing asks for its repositories' tags at once, and then for every tag's manifest at
    /// once, not each after the last has answered: here each of a stage's requests answers only
    /// once every other of that stage has been asked for.
    #[tokio::test]
    async fn a_list_asks_for_its_tags_and_their_manifests_at_once() {
        let every_tags = Arc::new(tokio::sync::Barrier::new(2));
        let tags = |tags: &'static [&'static str]| {
            let every_tags = Arc::clone(&every_tags);
            get(move || async move {
                every_tags.wait().await;
                Json(json!({ "tags": tags }))
            })
        };
        let every_manifest = Arc::new(tokio::sync::Barrier::new(3));
        let manifest = |digest: &'static str| {
            let every_manifest = Arc::clone(&every_manifest);
            head(move || async move {
                every_manifest.wait().await;
                [
                    (
                        header::CONTENT_TYPE.as_str(),
                        "application/vnd.oci.image.manifest.v1+json",
                    ),
                    ("docker-content-digest", digest),
                ]
                .into_response()
            })
        };
        let router = Router::new()
            .route(
                "/images/oci/v2/_catalog",
                get(|| async { Json(json!({"repositories": ["one", "two"]})) }),
            )
            .route("/images/oci/v2/one/tags/list", tags(&["a", "b"]))
            .route("/images/oci/v2/two/tags/list", tags(&["c"]))
            .route("/images/oci/v2/one/manifests/a", manifest("sha256:aa"))
            .route("/images/oci/v2/one/manifests/b", manifest("sha256:bb"))
            .route("/images/oci/v2/two/manifests/c", manifest("sha256:cc"));
        let client = piped(router);
        let listed = tokio::time::timeout(std::time::Duration::from_secs(60), list(&client))
            .await
            .expect("each stage's requests were asked for at once")
            .unwrap();
        assert_eq!(
            listed
                .iter()
                .map(|image| (image.reference.as_str(), image.digest.as_str()))
                .collect::<Vec<_>>(),
            [
                ("one:a", "sha256:aa"),
                ("one:b", "sha256:bb"),
                ("two:c", "sha256:cc")
            ]
        );
        client.close().await;
    }

    /// An image the server does not hold is refused `404`, `not_found`, by the reference it was
    /// asked for, by each call that takes one: what to branch on, as for a sandbox or a volume.
    /// Each call asks the registry for the image it was given, and nothing else.
    #[tokio::test]
    async fn an_image_the_server_does_not_hold_is_refused_404_by_its_reference() {
        let asked = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&asked);
        let unknown = move |request: Request| {
            let log = Arc::clone(&log);
            async move {
                let sent = format!("{} {}", request.method(), request.uri().path());
                log.lock().unwrap().push(sent);
                (
                    StatusCode::NOT_FOUND,
                    Json(json!({"errors": [{"code": "MANIFEST_UNKNOWN", "message": "unknown"}]})),
                )
            }
        };
        let client = piped(Router::new().fallback(unknown));
        let sent = || std::mem::take(&mut *asked.lock().unwrap());
        let digest = format!("sha256:{}", "0".repeat(64));
        let by_digest = format!("nope@{digest}");
        let by_digest_path = format!("DELETE /images/oci/v2/nope/manifests/{digest}");
        for (call, reference, result, sent, asked) in [
            (
                "untag",
                "nope:1",
                untag(&client, "nope:1").await,
                sent(),
                vec!["DELETE /images/oci/v2/nope/manifests/1"],
            ),
            (
                "remove",
                "nope:1",
                remove(&client, "nope:1").await,
                sent(),
                vec!["HEAD /images/oci/v2/nope/manifests/1"],
            ),
            (
                "remove",
                by_digest.as_str(),
                remove(&client, &by_digest).await,
                sent(),
                vec![by_digest_path.as_str()],
            ),
            (
                "tag",
                "nope:1",
                tag(&client, "nope:1", "other:1").await,
                sent(),
                vec!["GET /images/oci/v2/nope/manifests/1"],
            ),
        ] {
            assert_eq!(sent, asked, "{call} {reference}");
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

    /// A registry failure other than a `404` is left as it came: its status, and the request it
    /// answered.
    #[tokio::test]
    async fn a_registry_failure_other_than_404_is_left_as_it_came() {
        let failing = || async { (StatusCode::INTERNAL_SERVER_ERROR, "") };
        let client = piped(Router::new().fallback(failing));
        let refused = untag(&client, "app:1").await.unwrap_err();
        assert_eq!(refused.status(), Some(500), "{refused:?}");
        assert_eq!(
            refused.to_string(),
            "the server answered 500 Internal Server Error: DELETE app/manifests/1"
        );
        client.close().await;
    }
}
