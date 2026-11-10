//! The test server's images: an OCI registry in memory, served at `/images/oci/v2/`, as much of
//! one as the client uses.
//!
//! It pages its listings two names at a time, and mounts a blob from another repository only
//! when told it may.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex, MutexGuard},
};

use bytes::Bytes;
use http::{Method, Request, Response, StatusCode, header};
use http_body_util::{BodyExt, Full};
use serde_json::json;

use crate::image::{ImageName, ImagePruneOutput, oci::sha256};

pub(crate) const IMAGE_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
#[cfg(test)]
const IMAGE_INDEX: &str = "application/vnd.oci.image.index.v1+json";

#[derive(Default)]
pub(crate) struct State {
    /// By repository, then digest.
    pub(crate) blobs: BTreeMap<String, BTreeMap<String, Bytes>>,
    /// By repository, then digest: its type and bytes.
    pub(crate) manifests: BTreeMap<String, BTreeMap<String, (String, Bytes)>>,
    /// By repository, then tag: the digest it points at.
    pub(crate) tags: BTreeMap<String, BTreeMap<String, String>>,
    /// The repository each upload in progress is to, by its id.
    uploads: BTreeMap<String, String>,
    next_upload: u64,
    pub(crate) mounts: bool,
}

/// A registry in memory. Cheap to clone: every clone is the same registry.
#[derive(Clone, Default)]
pub(crate) struct Registry(Arc<Mutex<State>>);

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

/// The digests the manifest `bytes` names: its config and layers, or an index's manifests.
fn named(bytes: &[u8]) -> Vec<String> {
    serde_json::from_slice::<serde_json::Value>(bytes)
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
        .collect()
}

/// `reference`'s repository and tag, if it is `name:tag`.
fn split(reference: &str) -> Option<(&str, &str)> {
    reference
        .rsplit_once(':')
        .filter(|(name, tag)| !name.is_empty() && !tag.is_empty() && !tag.contains('/'))
}

impl Registry {
    pub(crate) fn state(&self) -> MutexGuard<'_, State> {
        self.0.lock().unwrap()
    }

    /// Store `bytes` as a blob in `name`, and answer its digest.
    pub(crate) fn blob(&self, name: &str, bytes: &[u8]) -> String {
        let digest = sha256(bytes);
        self.state()
            .blobs
            .entry(name.into())
            .or_default()
            .insert(digest.clone(), Bytes::copy_from_slice(bytes));
        digest
    }

    /// Store `manifest` of `media_type` in `name`, tagged `tag` if given, and answer its digest.
    pub(crate) fn manifest(
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
    pub(crate) fn image(&self, name: &str, tag: Option<&str>, content: &str) -> String {
        let config = serde_json::to_vec(&json!({ "content": content })).unwrap();
        let config_digest = self.blob(name, &config);
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
    #[cfg(test)]
    pub(crate) fn index(&self, name: &str, tag: &str) -> String {
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

    #[cfg(test)]
    pub(crate) fn tagged(&self, name: &str, tag: &str) -> Option<String> {
        self.state().tags.get(name)?.get(tag).cloned()
    }

    /// Point `reference`, as `name:tag`, at a new image holding `content`, and answer its digest:
    /// the same `content` is the same image. Why not, if `reference` is not `name:tag`.
    pub(crate) fn make(&self, reference: &str, content: &str) -> Result<String, String> {
        let (name, tag) =
            split(reference).ok_or_else(|| format!("{reference:?} is not name:tag"))?;
        Ok(self.image(name, Some(tag), content))
    }

    /// The digest `image` names, if it is here: a tag's, or a digest any repository holds.
    pub(crate) fn resolve(&self, image: &ImageName) -> Option<String> {
        let state = self.state();
        match image {
            ImageName::Tag { tag } => {
                let (name, tag) = split(tag)?;
                state.tags.get(name)?.get(tag).cloned()
            }
            ImageName::Digest { digest } => state
                .manifests
                .values()
                .any(|manifests| manifests.contains_key(digest))
                .then(|| digest.clone()),
        }
    }

    /// Delete each manifest no tag points at, no index kept names and no digest in `used` is,
    /// then each blob no manifest kept names, and answer what that deleted.
    pub(crate) fn prune(&self, used: &BTreeSet<String>) -> ImagePruneOutput {
        let mut state = self.state();
        let state = &mut *state;
        let mut images = BTreeSet::new();
        let mut reclaimed = 0;
        let names: BTreeSet<String> = state
            .manifests
            .keys()
            .chain(state.blobs.keys())
            .cloned()
            .collect();
        for name in names {
            let manifests = state.manifests.entry(name.clone()).or_default();
            let mut kept: BTreeSet<String> = state
                .tags
                .get(&name)
                .map(|tags| tags.values().cloned().collect())
                .unwrap_or_default();
            kept.extend(manifests.keys().filter(|d| used.contains(*d)).cloned());
            let mut blobs = BTreeSet::new();
            let mut walk: Vec<String> = kept.iter().cloned().collect();
            while let Some(digest) = walk.pop() {
                let Some((_, bytes)) = manifests.get(&digest) else {
                    continue;
                };
                for child in named(bytes) {
                    if manifests.contains_key(&child) {
                        if kept.insert(child.clone()) {
                            walk.push(child);
                        }
                    } else {
                        blobs.insert(child);
                    }
                }
            }
            manifests.retain(|digest, (_, bytes)| {
                let keep = kept.contains(digest);
                if !keep {
                    reclaimed += bytes.len() as u64;
                    images.insert(digest.clone());
                }
                keep
            });
            if manifests.is_empty() {
                state.manifests.remove(&name);
            }
            if let Some(stored) = state.blobs.get_mut(&name) {
                stored.retain(|digest, bytes| {
                    let keep = blobs.contains(digest);
                    if !keep {
                        reclaimed += bytes.len() as u64;
                    }
                    keep
                });
                if stored.is_empty() {
                    state.blobs.remove(&name);
                }
            }
        }
        ImagePruneOutput {
            images: images.into_iter().collect(),
            reclaimed,
        }
    }

    pub(crate) async fn handle<B>(&self, request: Request<B>) -> Answer
    where
        B: http_body::Body,
        B::Error: std::fmt::Debug,
    {
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
        let Some(path) = uri.path().strip_prefix("/images/oci/v2/") else {
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
                    let known = |d: &String| {
                        state.blobs.get(name).is_some_and(|b| b.contains_key(d))
                            || state.manifests.get(name).is_some_and(|m| m.contains_key(d))
                    };
                    if !named(&body).iter().all(known) {
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
                state.next_upload += 1;
                let id = format!("upload{}", state.next_upload);
                state.uploads.insert(id.clone(), name.into());
                let mut response = answer(StatusCode::ACCEPTED);
                let location = format!("/images/oci/v2/{name}/blobs/uploads/{id}");
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
}
