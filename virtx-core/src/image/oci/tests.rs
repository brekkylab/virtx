//! The image operations against a registry in memory, served at `/images/v2/` over an
//! in-memory HTTP/2 connection as stdio serves one.
//!
//! The registry pages its listings two names at a time, and mounts a blob from another
//! repository only when told it may.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::{Arc, Mutex},
};

use bytes::Bytes;
use futures_util::StreamExt;
use http::{Method, Request, Response, StatusCode, header};
use http_body_util::{BodyExt, Full};
use serde_json::json;

use super::manifest::sha256;
use crate::{
    client::Client,
    method::Event,
    transport::{TransportClient, serve_stdio},
};

const IMAGE_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
const IMAGE_INDEX: &str = "application/vnd.oci.image.index.v1+json";

#[derive(Default)]
struct State {
    /// By repository, then digest.
    blobs: BTreeMap<String, BTreeMap<String, Bytes>>,
    /// By repository, then digest: its type and bytes.
    manifests: BTreeMap<String, BTreeMap<String, (String, Bytes)>>,
    /// By repository, then tag: the digest it points at.
    tags: BTreeMap<String, BTreeMap<String, String>>,
    /// The repository each upload in progress is to.
    uploads: BTreeMap<String, String>,
    mounts: bool,
}

/// A registry in memory, as much of one as the client uses.
#[derive(Clone, Default)]
struct Registry(Arc<Mutex<State>>);

type Answer = Response<Full<Bytes>>;

fn answer(status: StatusCode) -> Answer {
    let mut response = Response::new(Full::new(Bytes::new()));
    *response.status_mut() = status;
    response
}

fn unknown(code: &str) -> Answer {
    let body = json!({"errors": [{"code": code, "message": "unknown"}]});
    let mut response = Response::new(Full::new(serde_json::to_vec(&body).unwrap().into()));
    *response.status_mut() = StatusCode::NOT_FOUND;
    response
}

/// `names` from after `last`, two of them, with a `Link` if there are more.
fn page(key: &str, names: Vec<String>, query: &BTreeMap<String, String>) -> Answer {
    let after: Vec<_> = names
        .into_iter()
        .filter(|name| query.get("last").is_none_or(|last| name > last))
        .collect();
    let page: Vec<_> = after.iter().take(2).cloned().collect();
    let mut response = Response::new(Full::new(
        serde_json::to_vec(&json!({ key: page })).unwrap().into(),
    ));
    if after.len() > 2 {
        response
            .headers_mut()
            .insert(header::LINK, "<next>; rel=\"next\"".parse().unwrap());
    }
    response
}

impl Registry {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.0.lock().unwrap()
    }

    /// Store `bytes` as a blob in `name`, and answer its digest.
    fn blob(&self, name: &str, bytes: &[u8]) -> String {
        let digest = sha256(bytes);
        self.state()
            .blobs
            .entry(name.into())
            .or_default()
            .insert(digest.clone(), Bytes::copy_from_slice(bytes));
        digest
    }

    /// Store `manifest` of `media_type` in `name`, tagged `tag` if given, and answer its digest.
    fn manifest(
        &self,
        name: &str,
        media_type: &str,
        manifest: serde_json::Value,
        tag: Option<&str>,
    ) -> String {
        let bytes = Bytes::from(serde_json::to_vec(&manifest).unwrap());
        let digest = sha256(&bytes);
        let mut state = self.state();
        state
            .manifests
            .entry(name.into())
            .or_default()
            .insert(digest.clone(), (media_type.into(), bytes));
        if let Some(tag) = tag {
            state
                .tags
                .entry(name.into())
                .or_default()
                .insert(tag.into(), digest.clone());
        }
        digest
    }

    /// An image of a config and one layer holding `content`, in `name` and tagged `tag`.
    fn image(&self, name: &str, tag: Option<&str>, content: &str) -> String {
        let config = format!("{{\"content\":\"{content}\"}}");
        let config_digest = self.blob(name, config.as_bytes());
        let layer_digest = self.blob(name, content.as_bytes());
        self.manifest(
            name,
            IMAGE_MANIFEST,
            json!({
                "schemaVersion": 2,
                "mediaType": IMAGE_MANIFEST,
                "config": {"mediaType": "application/vnd.oci.image.config.v1+json", "digest": config_digest, "size": config.len()},
                "layers": [{"mediaType": "application/vnd.oci.image.layer.v1.tar", "digest": layer_digest, "size": content.len()}],
            }),
            tag,
        )
    }

    /// An index of two images, in `name` and tagged `tag`.
    fn index(&self, name: &str, tag: &str) -> String {
        let manifests: Vec<_> = ["amd64", "arm64"]
            .into_iter()
            .map(|arch| {
                let digest = self.image(name, None, arch);
                let size = self.state().manifests[name][&digest].1.len();
                json!({"mediaType": IMAGE_MANIFEST, "digest": digest, "size": size,
                       "platform": {"os": "linux", "architecture": arch}})
            })
            .collect();
        self.manifest(
            name,
            IMAGE_INDEX,
            json!({"schemaVersion": 2, "mediaType": IMAGE_INDEX, "manifests": manifests}),
            Some(tag),
        )
    }

    fn tagged(&self, name: &str, tag: &str) -> Option<String> {
        self.state().tags.get(name)?.get(tag).cloned()
    }

    async fn handle(&self, request: Request<hyper::body::Incoming>) -> Answer {
        let method = request.method().clone();
        let uri = request.uri().clone();
        let query: BTreeMap<String, String> = uri
            .query()
            .unwrap_or("")
            .split('&')
            .filter_map(|pair| pair.split_once('='))
            .map(|(k, v)| (k.into(), v.into()))
            .collect();
        let headers = request.headers().clone();
        let body = request.into_body().collect().await.unwrap().to_bytes();
        let Some(path) = uri.path().strip_prefix("/images/v2/") else {
            return answer(StatusCode::NOT_FOUND);
        };
        let mut state = self.state();

        if path == "_catalog" {
            let names: BTreeSet<_> = state.manifests.keys().cloned().collect();
            return page("repositories", names.into_iter().collect(), &query);
        }
        if let Some(name) = path.strip_suffix("/tags/list") {
            let tags = state.tags.get(name).cloned().unwrap_or_default();
            return page("tags", tags.into_keys().collect(), &query);
        }
        if let Some((name, target)) = path.split_once("/manifests/") {
            let digest = state
                .tags
                .get(name)
                .and_then(|tags| tags.get(target))
                .cloned()
                .unwrap_or_else(|| target.to_owned());
            match method {
                Method::GET | Method::HEAD => {
                    let Some((media_type, bytes)) =
                        state.manifests.get(name).and_then(|m| m.get(&digest))
                    else {
                        return unknown("MANIFEST_UNKNOWN");
                    };
                    let mut response = Response::new(Full::new(if method == Method::GET {
                        bytes.clone()
                    } else {
                        Bytes::new()
                    }));
                    let headers = response.headers_mut();
                    headers.insert(header::CONTENT_TYPE, media_type.parse().unwrap());
                    headers.insert("docker-content-digest", digest.parse().unwrap());
                    return response;
                }
                Method::PUT => {
                    let media_type = headers[header::CONTENT_TYPE].to_str().unwrap().to_owned();
                    let digest = sha256(&body);
                    let names: Vec<String> = serde_json::from_slice::<serde_json::Value>(&body)
                        .unwrap()
                        .as_object()
                        .unwrap()
                        .iter()
                        .flat_map(|(key, value)| match key.as_str() {
                            "config" => vec![value.clone()],
                            "layers" | "manifests" => value.as_array().unwrap().clone(),
                            _ => vec![],
                        })
                        .map(|d| d["digest"].as_str().unwrap().to_owned())
                        .collect();
                    let known = |d: &String| {
                        state.blobs.get(name).is_some_and(|b| b.contains_key(d))
                            || state.manifests.get(name).is_some_and(|m| m.contains_key(d))
                    };
                    if !names.iter().all(known) {
                        return unknown("MANIFEST_BLOB_UNKNOWN");
                    }
                    if target != digest {
                        state
                            .tags
                            .entry(name.into())
                            .or_default()
                            .insert(target.into(), digest.clone());
                    }
                    state
                        .manifests
                        .entry(name.into())
                        .or_default()
                        .insert(digest, (media_type, body));
                    return answer(StatusCode::CREATED);
                }
                Method::DELETE if target == digest => {
                    let removed = state
                        .manifests
                        .get_mut(name)
                        .and_then(|m| m.remove(&digest));
                    if removed.is_none() {
                        return unknown("MANIFEST_UNKNOWN");
                    }
                    if let Some(tags) = state.tags.get_mut(name) {
                        tags.retain(|_, d| *d != digest);
                    }
                    return answer(StatusCode::ACCEPTED);
                }
                Method::DELETE => {
                    let removed = state.tags.get_mut(name).and_then(|t| t.remove(target));
                    return answer(if removed.is_some() {
                        StatusCode::ACCEPTED
                    } else {
                        StatusCode::NOT_FOUND
                    });
                }
                _ => return answer(StatusCode::METHOD_NOT_ALLOWED),
            }
        }
        if let Some((name, id)) = path.split_once("/blobs/uploads/") {
            if method == Method::POST {
                if let (Some(digest), Some(from)) = (query.get("mount"), query.get("from")) {
                    let blob = state.blobs.get(from).and_then(|b| b.get(digest)).cloned();
                    if let (true, Some(blob)) = (state.mounts, blob) {
                        state
                            .blobs
                            .entry(name.into())
                            .or_default()
                            .insert(digest.clone(), blob);
                        return answer(StatusCode::CREATED);
                    }
                }
                let id = uuid::Uuid::new_v4().to_string();
                state.uploads.insert(id.clone(), name.into());
                let mut response = answer(StatusCode::ACCEPTED);
                let location = format!("/images/v2/{name}/blobs/uploads/{id}");
                response
                    .headers_mut()
                    .insert(header::LOCATION, location.parse().unwrap());
                return response;
            }
            if state.uploads.remove(id).as_deref() != Some(name) {
                return unknown("BLOB_UPLOAD_UNKNOWN");
            }
            let digest = &query["digest"];
            if sha256(&body) != *digest {
                let mut response = unknown("DIGEST_INVALID");
                *response.status_mut() = StatusCode::BAD_REQUEST;
                return response;
            }
            state
                .blobs
                .entry(name.into())
                .or_default()
                .insert(digest.clone(), body);
            return answer(StatusCode::CREATED);
        }
        if let Some((name, digest)) = path.split_once("/blobs/") {
            let Some(blob) = state.blobs.get(name).and_then(|b| b.get(digest)) else {
                return unknown("BLOB_UNKNOWN");
            };
            return Response::new(Full::new(if method == Method::GET {
                blob.clone()
            } else {
                Bytes::new()
            }));
        }
        answer(StatusCode::NOT_FOUND)
    }

    /// A client of this registry over an in-memory pipe.
    fn client(&self) -> Client {
        let (client, server) = tokio::io::duplex(64 * 1024);
        let (input, output) = tokio::io::split(server);
        let registry = self.clone();
        let handle = move |request| {
            let registry = registry.clone();
            async move { registry.handle(request).await }
        };
        tokio::spawn(serve_stdio(handle, input, output));
        Client {
            inner: TransportClient::over(client),
        }
    }
}

/// A path in the system's temporary directory no other test uses.
fn temporary(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("virtx-{}-{name}", uuid::Uuid::new_v4()))
}

/// What a stream's events were: its lines, and its output.
async fn run<T>(
    events: impl futures_util::Stream<Item = Result<Event<T, String>, crate::Error>> + Unpin,
) -> Result<(Vec<String>, T), crate::Error> {
    let mut events = events;
    let mut lines = Vec::new();
    while let Some(event) = events.next().await {
        match event? {
            Event::Progress(progress) => lines.extend(progress.content),
            Event::Output(output) => return Ok((lines, output)),
        }
    }
    unreachable!("a stream ends with its output or its error")
}

#[tokio::test]
async fn every_tag_is_listed_with_its_digest_across_pages() {
    let registry = Registry::default();
    let alpine = registry.image("alpine", Some("3.20"), "alpine");
    // The guard is let go of before `manifest` takes it again.
    let manifest = serde_json::from_slice(&registry.state().manifests["alpine"][&alpine].1);
    registry.manifest("alpine", IMAGE_MANIFEST, manifest.unwrap(), Some("latest"));
    let base = registry.index("team/base", "1");
    let jq = registry.image("jq", Some("latest"), "jq");
    let client = registry.client();

    let listed: Vec<_> = client
        .list_image()
        .await
        .unwrap()
        .into_iter()
        .map(|image| (image.reference, image.digest))
        .collect();
    assert_eq!(
        listed,
        [
            ("alpine:3.20".to_owned(), alpine.clone()),
            ("alpine:latest".to_owned(), alpine),
            ("jq:latest".to_owned(), jq),
            ("team/base:1".to_owned(), base),
        ]
    );
}

#[tokio::test]
async fn a_tag_in_the_same_repository_points_at_the_same_manifest() {
    let registry = Registry::default();
    let digest = registry.image("alpine", Some("3.20"), "alpine");
    let client = registry.client();

    client
        .tag_image("alpine:3.20", "alpine:stable")
        .await
        .unwrap();
    assert_eq!(registry.tagged("alpine", "stable"), Some(digest.clone()));

    client
        .tag_image(format!("alpine@{digest}"), "alpine:pinned")
        .await
        .unwrap();
    assert_eq!(registry.tagged("alpine", "pinned"), Some(digest));
}

#[tokio::test]
async fn a_tag_in_another_repository_brings_what_the_image_names() {
    for mounts in [true, false] {
        let registry = Registry::default();
        registry.state().mounts = mounts;
        let digest = registry.index("team/base", "1");
        let client = registry.client();

        client.tag_image("team/base:1", "mine:1").await.unwrap();
        assert_eq!(registry.tagged("mine", "1"), Some(digest), "{mounts}");
        let state = registry.state();
        assert_eq!(state.manifests["mine"].len(), 3, "{mounts}");
        assert_eq!(state.blobs["mine"], state.blobs["team/base"], "{mounts}");
    }
}

#[tokio::test]
async fn untagging_leaves_the_image_and_removing_takes_every_tag_on_it() {
    let registry = Registry::default();
    let digest = registry.image("alpine", Some("3.20"), "alpine");
    let client = registry.client();
    client
        .tag_image("alpine:3.20", "alpine:latest")
        .await
        .unwrap();
    client
        .tag_image("alpine:3.20", "alpine:stable")
        .await
        .unwrap();

    client.untag_image("alpine:stable").await.unwrap();
    assert_eq!(registry.tagged("alpine", "stable"), None);
    assert!(registry.state().manifests["alpine"].contains_key(&digest));

    client.remove_image("alpine:latest").await.unwrap();
    assert_eq!(registry.tagged("alpine", "3.20"), None);
    assert!(!registry.state().manifests["alpine"].contains_key(&digest));

    assert!(
        client
            .untag_image(format!("alpine@{digest}"))
            .await
            .is_err()
    );
    assert!(client.remove_image("alpine:3.20").await.is_err());
}

#[tokio::test]
async fn a_downloaded_image_uploads_back_as_it_was() {
    let registry = Registry::default();
    let digest = registry.index("team/base", "1");
    let client = registry.client();
    let path = temporary("base.tar");

    let (lines, output) = run(client.download_image("team/base:1", &path))
        .await
        .unwrap();
    assert_eq!(output.digest, digest);
    // Two images of a config and a layer each, `arm64`'s and `amd64`'s.
    assert_eq!(lines.len(), 4, "{lines:?}");

    let mut archive = tar::Archive::new(std::fs::File::open(&path).unwrap());
    let mut index = None;
    let mut names = BTreeSet::new();
    for entry in archive.entries().unwrap() {
        let mut entry = entry.unwrap();
        let name = entry.path().unwrap().to_string_lossy().into_owned();
        if name == "index.json" {
            index = Some(serde_json::from_reader::<_, serde_json::Value>(&mut entry).unwrap());
        }
        names.insert(name);
    }
    let index = index.unwrap();
    assert_eq!(index["manifests"][0]["digest"], digest.as_str());
    assert_eq!(
        index["manifests"][0]["annotations"]["org.opencontainers.image.ref.name"],
        "1"
    );
    // `oci-layout`, `index.json`, and three manifests and four blobs.
    assert_eq!(names.len(), 9, "{names:?}");

    let (lines, output) = run(client.upload_image(&path, "copy:latest"))
        .await
        .unwrap();
    assert_eq!(output.digest, digest);
    assert!(
        lines.iter().all(|l| l.starts_with("uploading")),
        "{lines:?}"
    );
    assert_eq!(registry.tagged("copy", "latest"), Some(digest.clone()));
    {
        let state = registry.state();
        assert_eq!(state.blobs["copy"], state.blobs["team/base"]);
        assert_eq!(state.manifests["copy"], state.manifests["team/base"]);
    }

    let (lines, _) = run(client.upload_image(&path, "copy:again")).await.unwrap();
    assert!(
        lines.iter().all(|l| l.ends_with("is there already")),
        "{lines:?}"
    );
    std::fs::remove_file(&path).unwrap();
}

#[tokio::test]
async fn a_failed_download_writes_nothing() {
    let registry = Registry::default();
    let digest = registry.image("alpine", Some("3.20"), "alpine");
    let client = registry.client();
    let path = temporary("alpine.tar");

    assert!(
        run(client.download_image("missing:1", &path))
            .await
            .is_err()
    );
    assert!(!path.exists());

    // A layer whose bytes are not its digest's.
    let manifest: serde_json::Value =
        serde_json::from_slice(&registry.state().manifests["alpine"][&digest].1).unwrap();
    let layer = manifest["layers"][0]["digest"].as_str().unwrap().to_owned();
    registry
        .state()
        .blobs
        .get_mut("alpine")
        .unwrap()
        .insert(layer, Bytes::from_static(b"alpinf"));
    let error = run(client.download_image("alpine:3.20", &path))
        .await
        .err()
        .unwrap();
    assert!(error.to_string().contains("was read as"), "{error}");
    assert!(!path.exists());
    let partial = path.with_file_name(format!(
        "{}.partial",
        path.file_name().unwrap().to_string_lossy()
    ));
    assert!(!partial.exists());
}

#[tokio::test]
async fn an_upload_takes_the_image_its_archive_names_by_the_tag() {
    let registry = Registry::default();
    let one = registry.image("alpine", Some("1"), "one");
    let two = registry.image("alpine", Some("2"), "two");
    let client = registry.client();
    let (first, second) = (temporary("one.tar"), temporary("two.tar"));
    run(client.download_image("alpine:1", &first))
        .await
        .unwrap();
    run(client.download_image("alpine:2", &second))
        .await
        .unwrap();

    // One archive of both: the first's entries, then the second's blobs and an index of both.
    let path = temporary("both.tar");
    let mut both = tar::Builder::new(std::fs::File::create(&path).unwrap());
    let mut descriptors = Vec::new();
    for archive in [&first, &second] {
        let mut archive = tar::Archive::new(std::fs::File::open(archive).unwrap());
        for entry in archive.entries().unwrap() {
            let mut entry = entry.unwrap();
            let name = entry.path().unwrap().to_string_lossy().into_owned();
            let mut bytes = Vec::new();
            std::io::Read::read_to_end(&mut entry, &mut bytes).unwrap();
            match name.as_str() {
                "index.json" => {
                    let index: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                    descriptors.push(index["manifests"][0].clone());
                }
                "oci-layout" if !descriptors.is_empty() => {}
                _ => both
                    .append_data(&mut entry.header().clone(), &name, &bytes[..])
                    .unwrap(),
            }
        }
    }
    let index = serde_json::to_vec(&json!({"schemaVersion": 2, "manifests": descriptors})).unwrap();
    let mut header = tar::Header::new_ustar();
    header.set_size(index.len() as u64);
    header.set_mode(0o644);
    both.append_data(&mut header, "index.json", &index[..])
        .unwrap();
    both.finish().unwrap();

    let (_, output) = run(client.upload_image(&path, "other:2")).await.unwrap();
    assert_eq!(output.digest, two);
    let (_, output) = run(client.upload_image(&path, "other:1")).await.unwrap();
    assert_eq!(output.digest, one);
    let error = run(client.upload_image(&path, "other:3"))
        .await
        .err()
        .unwrap();
    assert!(error.to_string().contains("none named"), "{error}");

    for path in [first, second, path] {
        std::fs::remove_file(path).unwrap();
    }
}
