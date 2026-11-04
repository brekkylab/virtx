//! The registry this host serves its images as: the server end of the OCI distribution spec
//! (v1.1) over a [`Store`] -- its Pull, Push, Content Discovery and Content Management
//! requests, and `_catalog` beside them.
//!
//! An image here is its manifest, and is served as it is (see [`Store`]): each layer an EROFS
//! image, its blob the layer's file. Only such layers are taken: one pushed has to be an EROFS
//! image this host writes the same, so that the layer filed from it is the blob that was
//! pushed. Any other blob is a config, held as it is until the manifest naming it is put, and
//! then kept with the image. An index is not taken.
//!
//! A repository and a tag are a [`Reference`], as the store tags an image: `alpine` is
//! `docker.io/library/alpine`, as a pull of it is tagged. Blobs are the store's, whichever
//! repository they were put in.

use std::fs::File;
use std::io::{self, BufReader};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use bytes::Bytes;
use futures_util::StreamExt as _;
use http::{HeaderValue, Method, Request, Response, StatusCode, header};
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt as _, Empty, Full, StreamBody};
use oci_spec::image::{Descriptor, Digest, ImageManifest, MediaType, Sha256Digest};
use serde_json::json;
use sha2::{Digest as _, Sha256};
use tokio::io::AsyncWriteExt as _;
use tokio_util::io::ReaderStream;

use super::registry_client::Reference;
use super::store::{self, Store};
use crate::fstree::FsTree;

/// What a response carries.
pub type Body = UnsyncBoxBody<Bytes, io::Error>;

/// The largest manifest, or blob that is not a layer, taken: what the spec asks a registry to
/// take of a manifest at least.
const LIMIT: usize = 4 << 20;

/// How long a push may leave a blob in the temp directory unfinished, or a config there that
/// no manifest names, before it is dropped.
const UPLOAD_GRACE: Duration = Duration::from_secs(60 * 60);

/// What the temp directory a server keeps its pushes in is named after.
const UPLOADS: &str = "virtx-uvm-uploads-";

#[derive(Debug)]
pub struct RegistryServer {
    store: Store,
    /// The path the spec's `/v2/` is under, such as `/images`.
    base: String,
    /// Each blob being pushed, until it is whole, and each whole one that is not a layer, until
    /// a manifest names it: in the temp directory, and gone with this.
    uploads: PathBuf,
}

impl Drop for RegistryServer {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.uploads);
    }
}

/// A request refused, as the spec's `{"errors": [...]}` says it.
struct Refusal {
    status: StatusCode,
    code: &'static str,
    message: String,
}

fn refusal(status: StatusCode, code: &'static str, message: impl Into<String>) -> Refusal {
    Refusal {
        status,
        code,
        message: message.into(),
    }
}

impl From<io::Error> for Refusal {
    fn from(e: io::Error) -> Refusal {
        refusal(StatusCode::INTERNAL_SERVER_ERROR, "UNKNOWN", e.to_string())
    }
}

impl From<store::Error> for Refusal {
    fn from(e: store::Error) -> Refusal {
        refusal(StatusCode::INTERNAL_SERVER_ERROR, "UNKNOWN", e.to_string())
    }
}

impl From<tokio::task::JoinError> for Refusal {
    fn from(e: tokio::task::JoinError) -> Refusal {
        refusal(StatusCode::INTERNAL_SERVER_ERROR, "UNKNOWN", e.to_string())
    }
}

type Result<T> = std::result::Result<T, Refusal>;

impl RegistryServer {
    /// The registry of `store`, its `/v2/` under `base`: `/images` serves `/images/v2/`.
    pub fn new(store: Store, base: impl Into<String>) -> io::Result<RegistryServer> {
        static SERVERS: AtomicU64 = AtomicU64::new(0);
        let uploads = std::env::temp_dir().join(format!(
            "{UPLOADS}{}-{}",
            std::process::id(),
            SERVERS.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&uploads)?;
        Ok(RegistryServer {
            store,
            base: base.into(),
            uploads,
        })
    }

    /// Answer one request, whose path is under the base.
    pub async fn handle<B>(&self, request: Request<B>) -> Response<Body>
    where
        B: http_body::Body<Data = Bytes>,
        B::Error: std::fmt::Display,
    {
        let mut response = match self.answer(request).await {
            Ok(response) => response,
            Err(refused) => json_response(
                refused.status,
                &json!({"errors": [{"code": refused.code, "message": refused.message}]}),
            ),
        };
        response.headers_mut().insert(
            "docker-distribution-api-version",
            HeaderValue::from_static("registry/2.0"),
        );
        response
    }

    async fn answer<B>(&self, request: Request<B>) -> Result<Response<Body>>
    where
        B: http_body::Body<Data = Bytes>,
        B::Error: std::fmt::Display,
    {
        let unknown = || refusal(StatusCode::NOT_FOUND, "NAME_UNKNOWN", "no such endpoint");
        let path = request
            .uri()
            .path()
            .strip_prefix(self.base.as_str())
            .and_then(|path| path.strip_prefix("/v2"))
            .ok_or_else(unknown)?
            .to_owned();
        let query: Vec<(String, String)> =
            form_urlencoded::parse(request.uri().query().unwrap_or_default().as_bytes())
                .into_owned()
                .collect();
        let asked = |key: &str| {
            query
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.as_str())
        };
        let method = request.method().clone();

        if path.is_empty() || path == "/" {
            return match method {
                Method::GET | Method::HEAD => Ok(json_response(StatusCode::OK, &json!({}))),
                _ => Err(refusal(
                    StatusCode::METHOD_NOT_ALLOWED,
                    "UNSUPPORTED",
                    "the endpoint does not take this method",
                )),
            };
        }
        let path = path.strip_prefix('/').ok_or_else(unknown)?;
        if path == "_catalog" {
            return match method {
                Method::GET => self.catalog(asked("n"), asked("last")),
                _ => Err(refusal(
                    StatusCode::METHOD_NOT_ALLOWED,
                    "UNSUPPORTED",
                    "the endpoint does not take this method",
                )),
            };
        }

        // The name is everything before the endpoint, slashes and all.
        if let Some(name) = path.strip_suffix("/tags/list") {
            return match method {
                Method::GET => self.tags(name, asked("n"), asked("last")),
                _ => Err(refusal(
                    StatusCode::METHOD_NOT_ALLOWED,
                    "UNSUPPORTED",
                    "the endpoint does not take this method",
                )),
            };
        }
        if let Some(name) = path
            .strip_suffix("/blobs/uploads/")
            .or_else(|| path.strip_suffix("/blobs/uploads"))
        {
            return match method {
                Method::POST => {
                    let (digest, mount) = (asked("digest"), asked("mount"));
                    self.begin_upload(name, digest, mount, request.into_body())
                        .await
                }
                _ => Err(refusal(
                    StatusCode::METHOD_NOT_ALLOWED,
                    "UNSUPPORTED",
                    "the endpoint does not take this method",
                )),
            };
        }
        let (rest, last) = path.rsplit_once('/').ok_or_else(unknown)?;
        let (name, endpoint) = rest.rsplit_once('/').ok_or_else(unknown)?;
        match (endpoint, method) {
            ("manifests", Method::GET) => self.get_manifest(name, last, true).await,
            ("manifests", Method::HEAD) => self.get_manifest(name, last, false).await,
            ("manifests", Method::PUT) => self.put_manifest(name, last, request.into_body()).await,
            ("manifests", Method::DELETE) => self.delete_manifest(name, last),
            ("blobs", Method::GET) => self.get_blob(last, true).await,
            ("blobs", Method::HEAD) => self.get_blob(last, false).await,
            ("blobs", Method::DELETE) => self.delete_blob(last),
            ("referrers", Method::GET) => self.referrers(last, asked("artifactType")).await,
            ("uploads", method) => {
                let name = name.strip_suffix("/blobs").ok_or_else(unknown)?;
                let range = request
                    .headers()
                    .get(header::CONTENT_RANGE)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned);
                match method {
                    Method::GET => {
                        let len = std::fs::metadata(self.upload_path(last)?)?.len();
                        Ok(self.upload_accepted(name, last, StatusCode::NO_CONTENT, len))
                    }
                    Method::PATCH => {
                        self.patch_upload(name, last, range.as_deref(), request.into_body())
                            .await
                    }
                    Method::PUT => {
                        let digest = asked("digest");
                        self.put_upload(name, last, digest, request.into_body())
                            .await
                    }
                    Method::DELETE => {
                        std::fs::remove_file(self.upload_path(last)?)?;
                        Ok(empty(StatusCode::NO_CONTENT))
                    }
                    _ => Err(refusal(
                        StatusCode::METHOD_NOT_ALLOWED,
                        "UNSUPPORTED",
                        "the endpoint does not take this method",
                    )),
                }
            }
            _ => Err(unknown()),
        }
    }

    /// Every repository a tag is in, as the spec pages a tag listing.
    fn catalog(&self, n: Option<&str>, last: Option<&str>) -> Result<Response<Body>> {
        let mut repositories: Vec<String> = self
            .store
            .tags()?
            .iter()
            .map(|(reference, _)| repository(reference))
            .collect();
        repositories.sort();
        repositories.dedup();
        let (page, more) = paged(repositories, n, last)?;
        let mut response = json_response(StatusCode::OK, &json!({"repositories": page}));
        if more && let Some(last) = page.last() {
            let link = format!(
                "<{}/v2/_catalog?n={}&last={last}>; rel=\"next\"",
                self.base,
                page.len()
            );
            response.headers_mut().insert(
                header::LINK,
                HeaderValue::try_from(link).expect("names, digests and ids are ASCII"),
            );
        }
        Ok(response)
    }

    fn tags(&self, name: &str, n: Option<&str>, last: Option<&str>) -> Result<Response<Body>> {
        let wanted = repository(&tagged(name, "latest")?);
        let mut tags: Vec<String> = self
            .store
            .tags()?
            .iter()
            .filter(|(reference, _)| repository(reference) == wanted)
            .filter_map(|(reference, _)| reference.tag().map(str::to_owned))
            .collect();
        if tags.is_empty() {
            return Err(refusal(StatusCode::NOT_FOUND, "NAME_UNKNOWN", name));
        }
        tags.sort();
        let (page, more) = paged(tags, n, last)?;
        let mut response = json_response(StatusCode::OK, &json!({"name": name, "tags": page}));
        if more && let Some(last) = page.last() {
            let link = format!(
                "<{}/v2/{name}/tags/list?n={}&last={last}>; rel=\"next\"",
                self.base,
                page.len()
            );
            response.headers_mut().insert(
                header::LINK,
                HeaderValue::try_from(link).expect("names, digests and ids are ASCII"),
            );
        }
        Ok(response)
    }

    async fn get_manifest(
        &self,
        name: &str,
        reference: &str,
        with_body: bool,
    ) -> Result<Response<Body>> {
        let unknown = || refusal(StatusCode::NOT_FOUND, "MANIFEST_UNKNOWN", reference);
        let image = if reference.contains(':') {
            reference
                .parse::<Digest>()
                .map_err(|e| refusal(StatusCode::BAD_REQUEST, "DIGEST_INVALID", e.to_string()))?
        } else {
            self.store
                .get_tag(&tagged(name, reference)?)?
                .ok_or_else(unknown)?
        };
        let (body, _) = self.store.get_image_manifest(&image)?.ok_or_else(unknown)?;

        let mut response = Response::new(if with_body {
            Full::new(Bytes::from(body.clone()))
                .map_err(|never| match never {})
                .boxed_unsync()
        } else {
            Empty::new().map_err(|never| match never {}).boxed_unsync()
        });
        let headers = response.headers_mut();
        // What the manifest says it is; it was taken whatever it said.
        let media_type = serde_json::from_slice::<serde_json::Value>(&body)
            .ok()
            .and_then(|manifest| manifest["mediaType"].as_str().map(str::to_owned))
            .and_then(|media_type| HeaderValue::try_from(media_type).ok())
            .unwrap_or_else(|| {
                HeaderValue::from_static("application/vnd.oci.image.manifest.v1+json")
            });
        headers.insert(header::CONTENT_TYPE, media_type);
        headers.insert(header::CONTENT_LENGTH, HeaderValue::from(body.len()));
        headers.insert(
            "docker-content-digest",
            HeaderValue::try_from(image.to_string()).expect("names, digests and ids are ASCII"),
        );
        Ok(response)
    }

    async fn put_manifest<B>(&self, name: &str, reference: &str, body: B) -> Result<Response<Body>>
    where
        B: http_body::Body<Data = Bytes>,
        B::Error: std::fmt::Display,
    {
        let invalid = |what: String| refusal(StatusCode::BAD_REQUEST, "MANIFEST_INVALID", what);

        let mut read = Vec::new();
        let mut body = std::pin::pin!(body);
        while let Some(frame) = body.frame().await {
            let frame = frame.map_err(|e| invalid(e.to_string()))?;
            if let Ok(data) = frame.into_data() {
                read.extend_from_slice(&data);
            }
            if read.len() > LIMIT {
                return Err(refusal(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "SIZE_INVALID",
                    format!("a manifest is at most {LIMIT} bytes"),
                ));
            }
        }
        let body = read;
        let digest = sha256(&body);
        let tag = if reference.contains(':') {
            if reference != digest.to_string() {
                return Err(refusal(
                    StatusCode::BAD_REQUEST,
                    "DIGEST_INVALID",
                    format!("the manifest is {digest}, not {reference}"),
                ));
            }
            None
        } else {
            Some(tagged(name, reference)?)
        };

        let parsed: serde_json::Value =
            serde_json::from_slice(&body).map_err(|e| invalid(e.to_string()))?;
        if parsed.get("manifests").is_some() {
            return Err(invalid("an index is not taken here".into()));
        }
        let manifest: ImageManifest =
            serde_json::from_slice(&body).map_err(|e| invalid(e.to_string()))?;
        let config = self.config(manifest.config().digest())?.ok_or_else(|| {
            refusal(
                StatusCode::NOT_FOUND,
                "MANIFEST_BLOB_UNKNOWN",
                manifest.config().digest().to_string(),
            )
        })?;
        self.store
            .insert_image_manifest(&body, &config)
            .map_err(|e| match e {
                store::Error::Missing(missing) => refusal(
                    StatusCode::NOT_FOUND,
                    "MANIFEST_BLOB_UNKNOWN",
                    missing.to_string(),
                ),
                store::Error::Malformed(what) => invalid(what),
                e => e.into(),
            })?;
        // Kept with the image now.
        let _ = std::fs::remove_file(self.uploads.join(manifest.config().digest().digest()));
        if let Some(tag) = tag {
            self.store.insert_tag(&tag, &digest)?;
        }

        let mut response = empty(StatusCode::CREATED);
        let headers = response.headers_mut();
        headers.insert(
            header::LOCATION,
            HeaderValue::try_from(format!("{}/v2/{name}/manifests/{digest}", self.base))
                .expect("names, digests and ids are ASCII"),
        );
        headers.insert(
            "docker-content-digest",
            HeaderValue::try_from(digest.to_string()).expect("names, digests and ids are ASCII"),
        );
        if let Some(subject) = parsed["subject"]["digest"].as_str()
            && let Ok(subject) = HeaderValue::try_from(subject)
        {
            headers.insert("oci-subject", subject);
        }
        Ok(response)
    }

    /// A tag takes only itself away; a digest takes every tag naming its image in this
    /// repository, and the image too once no tag in any other names it.
    fn delete_manifest(&self, name: &str, reference: &str) -> Result<Response<Body>> {
        let unknown = || refusal(StatusCode::NOT_FOUND, "MANIFEST_UNKNOWN", reference);
        if !reference.contains(':') {
            self.store
                .remove_tag(&tagged(name, reference)?)?
                .ok_or_else(unknown)?;
            return Ok(empty(StatusCode::ACCEPTED));
        }
        let digest = reference
            .parse::<Digest>()
            .map_err(|e| refusal(StatusCode::BAD_REQUEST, "DIGEST_INVALID", e.to_string()))?;
        if self.store.get_image_manifest(&digest)?.is_none() {
            return Err(unknown());
        }
        let here = repository(&tagged(name, "latest")?);
        let (tags_here, tags_elsewhere): (Vec<_>, Vec<_>) = self
            .store
            .tags()?
            .into_iter()
            .filter(|(_, named)| *named == digest)
            .partition(|(tag, _)| repository(tag) == here);
        // An image tagged only in other repositories is not this one's.
        if tags_here.is_empty() && !tags_elsewhere.is_empty() {
            return Err(unknown());
        }
        for (tag, _) in tags_here {
            self.store.remove_tag(&tag)?;
        }
        if !tags_elsewhere.is_empty() {
            return Ok(empty(StatusCode::ACCEPTED));
        }
        self.store.remove_image(&digest).map_err(|e| match e {
            store::Error::InUse(..) => {
                refusal(StatusCode::METHOD_NOT_ALLOWED, "DENIED", e.to_string())
            }
            e => e.into(),
        })?;
        Ok(empty(StatusCode::ACCEPTED))
    }

    /// A layer, streamed from its file, or a config.
    async fn get_blob(&self, digest: &str, with_body: bool) -> Result<Response<Body>> {
        let blob = blob_digest(digest)?;
        let (len, body) = match tokio::fs::File::open(self.store.layer_path(&blob)).await {
            Ok(file) => (
                file.metadata().await?.len(),
                StreamBody::new(
                    ReaderStream::new(file).map(|chunk| chunk.map(http_body::Frame::data)),
                )
                .boxed_unsync(),
            ),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let config = self
                    .config(&blob)?
                    .ok_or_else(|| refusal(StatusCode::NOT_FOUND, "BLOB_UNKNOWN", digest))?;
                (
                    config.len() as u64,
                    Full::new(Bytes::from(config))
                        .map_err(|never| match never {})
                        .boxed_unsync(),
                )
            }
            Err(e) => return Err(e.into()),
        };

        let mut response = Response::new(if with_body {
            body
        } else {
            Empty::new().map_err(|never| match never {}).boxed_unsync()
        });
        let headers = response.headers_mut();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/octet-stream"),
        );
        headers.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
        headers.insert(
            "docker-content-digest",
            HeaderValue::try_from(blob.to_string()).expect("names, digests and ids are ASCII"),
        );
        Ok(response)
    }

    /// A layer no image stacks, or a config no manifest names yet. One an image keeps goes with
    /// the image.
    fn delete_blob(&self, digest: &str) -> Result<Response<Body>> {
        let blob = blob_digest(digest)?;
        let denied = |e: store::Error| match e {
            store::Error::InUse(..) => {
                refusal(StatusCode::METHOD_NOT_ALLOWED, "DENIED", e.to_string())
            }
            e => e.into(),
        };
        if self.store.contains_layer(&blob) {
            self.store.remove_layer(&blob).map_err(denied)?;
            return Ok(empty(StatusCode::ACCEPTED));
        }
        match std::fs::remove_file(self.uploads.join(blob.digest())) {
            Ok(()) => return Ok(empty(StatusCode::ACCEPTED)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        if self.config(&blob)?.is_some() {
            return Err(refusal(
                StatusCode::METHOD_NOT_ALLOWED,
                "DENIED",
                format!("{digest} is an image's config"),
            ));
        }
        Err(refusal(StatusCode::NOT_FOUND, "BLOB_UNKNOWN", digest))
    }

    /// `POST .../blobs/uploads/`: a blob put whole with `digest`, one mounted from wherever it
    /// is here, or an upload begun -- which a mount of a blob not here is too.
    async fn begin_upload<B>(
        &self,
        name: &str,
        digest: Option<&str>,
        mount: Option<&str>,
        body: B,
    ) -> Result<Response<Body>>
    where
        B: http_body::Body<Data = Bytes>,
        B::Error: std::fmt::Display,
    {
        tagged(name, "latest")?;
        if let Some(mount) = mount {
            let blob = blob_digest(mount)?;
            if self.store.contains_layer(&blob) || self.config(&blob)?.is_some() {
                return Ok(self.blob_created(name, &blob));
            }
        }

        let uploads = self.uploads.clone();
        tokio::task::spawn_blocking(move || sweep(&uploads)).await?;
        let id = uuid::Uuid::new_v4().simple().to_string();
        let path = self.uploads.join(&id);
        // Another server's sweep takes a directory it finds empty and old, this one's too.
        tokio::fs::create_dir_all(&self.uploads).await?;
        tokio::fs::File::create(&path).await?;
        if let Some(digest) = digest {
            append(&path, body).await?;
            return self.finish(name, &path, digest).await;
        }
        Ok(self.upload_accepted(name, &id, StatusCode::ACCEPTED, 0))
    }

    /// `PATCH`: the next chunk, which has to start where the upload has got to.
    async fn patch_upload<B>(
        &self,
        name: &str,
        id: &str,
        range: Option<&str>,
        body: B,
    ) -> Result<Response<Body>>
    where
        B: http_body::Body<Data = Bytes>,
        B::Error: std::fmt::Display,
    {
        let path = self.upload_path(id)?;
        let len = tokio::fs::metadata(&path).await?.len();
        if let Some(range) = range {
            let start = range
                .split_once('-')
                .and_then(|(start, _)| start.trim().parse::<u64>().ok());
            if start != Some(len) {
                return Ok(self.upload_accepted(name, id, StatusCode::RANGE_NOT_SATISFIABLE, len));
            }
        }
        let len = append(&path, body).await?;
        Ok(self.upload_accepted(name, id, StatusCode::ACCEPTED, len))
    }

    /// `PUT`: the last chunk, if any, and the digest the whole is.
    async fn put_upload<B>(
        &self,
        name: &str,
        id: &str,
        digest: Option<&str>,
        body: B,
    ) -> Result<Response<Body>>
    where
        B: http_body::Body<Data = Bytes>,
        B::Error: std::fmt::Display,
    {
        let path = self.upload_path(id)?;
        append(&path, body).await?;
        let digest = digest
            .ok_or_else(|| refusal(StatusCode::BAD_REQUEST, "DIGEST_INVALID", "no digest given"))?;
        self.finish(name, &path, digest).await
    }

    /// File the whole upload at `path`, if it is `digest`: an EROFS image as a layer, anything
    /// else as a config. The upload is gone either way.
    async fn finish(&self, name: &str, path: &Path, digest: &str) -> Result<Response<Body>> {
        let (store, uploads) = (self.store.clone(), self.uploads.clone());
        let (upload, digest) = (path.to_owned(), digest.to_owned());
        // Reading the whole of it is blocking work.
        let filed = tokio::task::spawn_blocking(move || {
            let filed = file_upload(&store, &uploads, &upload, &digest);
            let _ = std::fs::remove_file(&upload);
            filed
        })
        .await??;
        Ok(self.blob_created(name, &filed))
    }

    /// Every manifest here whose `subject` is `digest`, as an index of them.
    async fn referrers(&self, digest: &str, artifact_type: Option<&str>) -> Result<Response<Body>> {
        let subject = digest
            .parse::<Digest>()
            .map_err(|e| refusal(StatusCode::BAD_REQUEST, "DIGEST_INVALID", e.to_string()))?;
        let store = self.store.clone();
        let wanted = artifact_type.map(str::to_owned);
        // Every image's manifest is read: blocking work.
        let manifests = tokio::task::spawn_blocking(move || -> Result<Vec<serde_json::Value>> {
            let mut found = Vec::new();
            for image in store.images()? {
                let Some((body, _)) = store.get_image_manifest(&image)? else {
                    continue;
                };
                let Ok(manifest) = serde_json::from_slice::<ImageManifest>(&body) else {
                    continue;
                };
                if manifest.subject().as_ref().map(Descriptor::digest) != Some(&subject) {
                    continue;
                }
                let artifact_type = manifest
                    .artifact_type()
                    .clone()
                    .unwrap_or_else(|| manifest.config().media_type().clone())
                    .to_string();
                if wanted
                    .as_ref()
                    .is_some_and(|wanted| *wanted != artifact_type)
                {
                    continue;
                }
                let mut descriptor = json!({
                    "mediaType": manifest
                        .media_type()
                        .clone()
                        .unwrap_or(MediaType::ImageManifest)
                        .to_string(),
                    "size": body.len(),
                    "digest": image.to_string(),
                    "artifactType": artifact_type,
                });
                if let Some(annotations) = manifest.annotations() {
                    descriptor["annotations"] = json!(annotations);
                }
                found.push(descriptor);
            }
            Ok(found)
        })
        .await??;

        let index_type = MediaType::ImageIndex.to_string();
        let index = json!({
            "schemaVersion": 2,
            "mediaType": index_type,
            "manifests": manifests,
        });
        let mut response = json_response(StatusCode::OK, &index);
        let headers = response.headers_mut();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::try_from(&index_type).expect("names, digests and ids are ASCII"),
        );
        if artifact_type.is_some() {
            headers.insert(
                "oci-filters-applied",
                HeaderValue::from_static("artifactType"),
            );
        }
        Ok(response)
    }

    /// The config `blob`: one waiting for its manifest, or one an image keeps.
    fn config(&self, blob: &Digest) -> Result<Option<Vec<u8>>> {
        match std::fs::read(self.uploads.join(blob.digest())) {
            Ok(config) => return Ok(Some(config)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        for image in self.store.images()? {
            if let Some((_, config)) = self.store.get_image_manifest(&image)?
                && sha256(&config) == *blob
            {
                return Ok(Some(config));
            }
        }
        Ok(None)
    }

    /// The file the upload `id` is written to. Nothing but an id this gave is taken for one,
    /// since it becomes a path.
    fn upload_path(&self, id: &str) -> Result<PathBuf> {
        let path = self.uploads.join(id);
        if id.len() != 32 || !id.bytes().all(|b| b.is_ascii_hexdigit()) || !path.is_file() {
            return Err(refusal(StatusCode::NOT_FOUND, "BLOB_UPLOAD_UNKNOWN", id));
        }
        Ok(path)
    }

    /// Where an upload has got to: `len` bytes of it, which is what `Range` says.
    fn upload_accepted(
        &self,
        name: &str,
        id: &str,
        status: StatusCode,
        len: u64,
    ) -> Response<Body> {
        let mut response = empty(status);
        let headers = response.headers_mut();
        headers.insert(
            header::LOCATION,
            HeaderValue::try_from(format!("{}/v2/{name}/blobs/uploads/{id}", self.base))
                .expect("names, digests and ids are ASCII"),
        );
        headers.insert(
            header::RANGE,
            HeaderValue::try_from(format!("0-{}", len.saturating_sub(1)))
                .expect("names, digests and ids are ASCII"),
        );
        headers.insert(
            "docker-upload-uuid",
            HeaderValue::try_from(id).expect("names, digests and ids are ASCII"),
        );
        response
    }

    fn blob_created(&self, name: &str, blob: &Digest) -> Response<Body> {
        let mut response = empty(StatusCode::CREATED);
        let headers = response.headers_mut();
        headers.insert(
            header::LOCATION,
            HeaderValue::try_from(format!("{}/v2/{name}/blobs/{blob}", self.base))
                .expect("names, digests and ids are ASCII"),
        );
        headers.insert(
            "docker-content-digest",
            HeaderValue::try_from(blob.to_string()).expect("names, digests and ids are ASCII"),
        );
        response
    }
}

/// Drop what pushes left in the temp directory and never finished once it is [`UPLOAD_GRACE`]
/// old -- uploads, and configs no manifest named -- this server's or any other's, and a
/// directory left empty, which is a server's that is gone or has nothing in hand.
fn sweep(uploads: &Path) {
    let old = |path: &Path| {
        std::fs::metadata(path)
            .and_then(|metadata| metadata.modified())
            .is_ok_and(|modified| modified.elapsed().is_ok_and(|age| age >= UPLOAD_GRACE))
    };
    let Some(temp) = uploads.parent() else {
        return;
    };
    let Ok(servers) = std::fs::read_dir(temp) else {
        return;
    };
    for server in servers.flatten() {
        if !server.file_name().to_string_lossy().starts_with(UPLOADS) {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(server.path()) else {
            continue;
        };
        for entry in entries.flatten() {
            if old(&entry.path()) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
        if server.path() != uploads && old(&server.path()) {
            // Refused unless it is empty.
            let _ = std::fs::remove_dir(server.path());
        }
    }
}

/// File the upload at `path`, which has to be `digest`: as a layer if it is an EROFS image this
/// host writes the same -- the layer is then the blob -- else, if it is small enough to be a
/// config, in `uploads` until a manifest names it.
fn file_upload(store: &Store, uploads: &Path, path: &Path, digest: &str) -> Result<Digest> {
    let invalid = |what: String| refusal(StatusCode::BAD_REQUEST, "DIGEST_INVALID", what);
    let blob = blob_digest(digest)?;
    let mut hasher = Sha256::new();
    io::copy(&mut File::open(path)?, &mut hasher)?;
    if format!("{:x}", hasher.finalize()) != blob.digest() {
        return Err(invalid(format!("the blob is not {digest}")));
    }
    if store.contains_layer(&blob) {
        return Ok(blob);
    }

    if let Ok(tree) = FsTree::from_erofs(BufReader::new(File::open(path)?)) {
        let layer = store.insert_layer(&tree)?;
        if layer != blob {
            let _ = store.remove_layer(&layer);
            return Err(refusal(
                StatusCode::BAD_REQUEST,
                "BLOB_UPLOAD_INVALID",
                format!("{digest} is an EROFS image, but not as this host writes it"),
            ));
        }
        return Ok(blob);
    }
    if std::fs::metadata(path)?.len() > LIMIT as u64 {
        return Err(refusal(
            StatusCode::BAD_REQUEST,
            "BLOB_UPLOAD_INVALID",
            format!("{digest} is not an EROFS image, and too large to be a config"),
        ));
    }
    std::fs::rename(path, uploads.join(blob.digest()))?;
    Ok(blob)
}

/// `name:tag` as the store names it, which also checks both are what the spec takes.
fn tagged(name: &str, tag: &str) -> Result<Reference> {
    // A name in a path never has a port or a digest in it: the spec's grammar has neither.
    if name.contains([':', '@']) {
        return Err(refusal(StatusCode::BAD_REQUEST, "NAME_INVALID", name));
    }
    format!("{name}:{tag}")
        .parse::<Reference>()
        .map_err(|e| refusal(StatusCode::BAD_REQUEST, "NAME_INVALID", e.to_string()))
}

/// The repository a reference is in, as `_catalog` lists it and a path names it back.
fn repository(reference: &Reference) -> String {
    format!("{}/{}", reference.registry(), reference.repository())
}

/// A blob's digest. Only sha256 is kept here.
fn blob_digest(text: &str) -> Result<Digest> {
    let digest = text
        .parse::<Digest>()
        .map_err(|e| refusal(StatusCode::BAD_REQUEST, "DIGEST_INVALID", e.to_string()))?;
    if digest.algorithm().as_ref() != "sha256" {
        return Err(refusal(
            StatusCode::BAD_REQUEST,
            "DIGEST_INVALID",
            format!("{text}: only sha256 is taken"),
        ));
    }
    Ok(digest)
}

fn sha256(bytes: &[u8]) -> Digest {
    format!("{:x}", Sha256::digest(bytes))
        .parse::<Sha256Digest>()
        .expect("sha2 writes 64 lowercase hex characters")
        .into()
}

/// The page of sorted `items` after `last`, at most `n` long, and whether more follow.
fn paged(items: Vec<String>, n: Option<&str>, last: Option<&str>) -> Result<(Vec<String>, bool)> {
    let after: Vec<String> = match last {
        Some(last) => items
            .into_iter()
            .filter(|item| item.as_str() > last)
            .collect(),
        None => items,
    };
    let Some(n) = n else {
        return Ok((after, false));
    };
    let n: usize = n
        .parse()
        .map_err(|_| refusal(StatusCode::BAD_REQUEST, "PAGINATION_NUMBER_INVALID", n))?;
    let more = after.len() > n;
    Ok((after.into_iter().take(n).collect(), more))
}

/// Append what `body` carries to the upload at `path`, and answer with how long it is now.
async fn append<B>(path: &Path, body: B) -> Result<u64>
where
    B: http_body::Body<Data = Bytes>,
    B::Error: std::fmt::Display,
{
    let mut file = tokio::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .await?;
    let mut body = std::pin::pin!(body);
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|e| {
            refusal(
                StatusCode::BAD_REQUEST,
                "BLOB_UPLOAD_INVALID",
                e.to_string(),
            )
        })?;
        if let Ok(data) = frame.into_data() {
            file.write_all(&data).await?;
        }
    }
    file.flush().await?;
    Ok(file.metadata().await?.len())
}

fn empty(status: StatusCode) -> Response<Body> {
    let mut response = Response::new(Empty::new().map_err(|never| match never {}).boxed_unsync());
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(header::CONTENT_LENGTH, HeaderValue::from(0));
    response
}

fn json_response(status: StatusCode, body: &serde_json::Value) -> Response<Body> {
    let mut response = Response::new(
        Full::new(Bytes::from(body.to_string()))
            .map_err(|never| match never {})
            .boxed_unsync(),
    );
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
}

#[cfg(test)]
mod tests {
    use http::HeaderMap;
    use oci_spec::image::{ImageConfiguration, ImageManifestBuilder};

    use super::*;
    use crate::fstree::{Entry, Meta};
    use crate::image::store::EROFS_LAYER;

    struct Scratch(Store, RegistryServer);

    impl Scratch {
        fn new(name: &str) -> Scratch {
            let path = std::env::temp_dir()
                .join(format!("virtx-uvm-registry-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            let store = Store::new(path).unwrap();
            let server = RegistryServer::new(store.clone(), "/images").unwrap();
            Scratch(store, server)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(self.0.root());
        }
    }

    async fn send(
        server: &RegistryServer,
        method: Method,
        uri: &str,
        body: &[u8],
    ) -> (StatusCode, HeaderMap, Vec<u8>) {
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .body(Full::new(Bytes::copy_from_slice(body)))
            .unwrap();
        let response = server.handle(request).await;
        let (parts, body) = response.into_parts();
        let body = body.collect().await.unwrap().to_bytes().to_vec();
        (parts.status, parts.headers, body)
    }

    fn header<'a>(headers: &'a HeaderMap, name: &str) -> &'a str {
        headers.get(name).unwrap().to_str().unwrap()
    }

    /// A layer as a client of this registry has it: the EROFS image this host writes.
    fn erofs(content: &[u8]) -> Vec<u8> {
        let mut tree = FsTree::new();
        tree.insert(
            b"etc/hostname",
            Entry::File {
                meta: Meta::default(),
                data: content.to_vec(),
            },
        )
        .unwrap();
        let mut image = io::Cursor::new(Vec::new());
        tree.to_erofs(&mut image).unwrap();
        image.into_inner()
    }

    fn manifest(config: &[u8], layers: &[(&str, &[u8])]) -> Vec<u8> {
        let manifest = ImageManifestBuilder::default()
            .schema_version(2u32)
            .media_type(MediaType::ImageManifest)
            .config(Descriptor::new(
                MediaType::ImageConfig,
                config.len() as u64,
                sha256(config),
            ))
            .layers(
                layers
                    .iter()
                    .map(|(media_type, layer)| {
                        Descriptor::new(
                            MediaType::Other((*media_type).into()),
                            layer.len() as u64,
                            sha256(layer),
                        )
                    })
                    .collect::<Vec<_>>(),
            )
            .build()
            .unwrap();
        serde_json::to_vec(&manifest).unwrap()
    }

    fn config(layers: &[&[u8]]) -> Vec<u8> {
        let diff_ids: Vec<String> = layers.iter().map(|l| sha256(l).to_string()).collect();
        serde_json::to_vec(&json!({
            "architecture": "arm64",
            "os": "linux",
            "rootfs": {"type": "layers", "diff_ids": diff_ids},
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn what_is_pushed_comes_back_as_it_was() {
        let scratch = Scratch::new("push");
        let server = &scratch.1;
        let layer = erofs(b"pushed");
        let config = config(&[&layer]);
        let manifest = manifest(&config, &[(EROFS_LAYER, &layer)]);

        // The layer in two chunks, the config whole.
        let (status, headers, _) = send(
            server,
            Method::POST,
            "/images/v2/team/app/blobs/uploads/",
            b"",
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED);
        let location = header(&headers, "location").to_owned();
        let (half, rest) = layer.split_at(layer.len() / 2);
        let (status, headers, _) = send(server, Method::PATCH, &location, half).await;
        assert_eq!(status, StatusCode::ACCEPTED);
        assert_eq!(header(&headers, "range"), format!("0-{}", half.len() - 1));
        let put = format!("{location}?digest={}", sha256(&layer));
        let (status, _, _) = send(server, Method::PUT, &put, rest).await;
        assert_eq!(status, StatusCode::CREATED);
        let post = format!(
            "/images/v2/team/app/blobs/uploads/?digest={}",
            sha256(&config)
        );
        let (status, _, _) = send(server, Method::POST, &post, &config).await;
        assert_eq!(status, StatusCode::CREATED);

        let (status, headers, _) = send(
            server,
            Method::PUT,
            "/images/v2/team/app/manifests/v1",
            &manifest,
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(
            header(&headers, "docker-content-digest"),
            sha256(&manifest).to_string()
        );

        let by_tag = send(server, Method::GET, "/images/v2/team/app/manifests/v1", b"").await;
        assert_eq!((by_tag.0, &by_tag.2), (StatusCode::OK, &manifest));
        let by_digest = format!("/images/v2/team/app/manifests/{}", sha256(&manifest));
        let (status, _, body) = send(server, Method::GET, &by_digest, b"").await;
        assert_eq!((status, body), (StatusCode::OK, manifest.clone()));
        let blob = format!("/images/v2/team/app/blobs/{}", sha256(&layer));
        let (status, _, body) = send(server, Method::GET, &blob, b"").await;
        assert_eq!((status, body), (StatusCode::OK, layer.clone()));

        let (_, _, body) = send(server, Method::GET, "/images/v2/team/app/tags/list", b"").await;
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap()["tags"],
            json!(["v1"])
        );
        let (_, _, body) = send(server, Method::GET, "/images/v2/_catalog", b"").await;
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap()["repositories"],
            json!(["docker.io/team/app"])
        );

        let (status, _, _) = send(server, Method::DELETE, &by_digest, b"").await;
        assert_eq!(status, StatusCode::ACCEPTED);
        let (status, _, _) =
            send(server, Method::GET, "/images/v2/team/app/manifests/v1", b"").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    /// An image pulled or built is served as a manifest made for it, the same each time.
    #[tokio::test]
    async fn an_image_made_here_is_served_as_its_layers() {
        let scratch = Scratch::new("export");
        let (store, server) = (&scratch.0, &scratch.1);
        let mut tree = FsTree::new();
        tree.insert(
            b"bin",
            Entry::Directory {
                meta: Meta::default(),
                opaque: false,
            },
        )
        .unwrap();
        let layer = store.insert_layer(&tree).unwrap();
        let image = store
            .insert_image(&ImageConfiguration::default(), std::slice::from_ref(&layer))
            .unwrap();
        store
            .insert_tag(&"alpine:3.21".parse().unwrap(), &image)
            .unwrap();

        let (status, headers, body) =
            send(server, Method::GET, "/images/v2/alpine/manifests/3.21", b"").await;
        assert_eq!(status, StatusCode::OK);
        let digest = header(&headers, "docker-content-digest").to_owned();
        let manifest: ImageManifest = serde_json::from_slice(&body).unwrap();
        assert_eq!(manifest.layers()[0].digest(), &layer);
        assert_eq!(manifest.layers()[0].media_type().to_string(), EROFS_LAYER);

        let blob = format!("/images/v2/alpine/blobs/{layer}");
        let (_, _, served) = send(server, Method::GET, &blob, b"").await;
        assert_eq!(sha256(&served), layer);
        let blob = format!("/images/v2/alpine/blobs/{}", manifest.config().digest());
        let (_, _, served) = send(server, Method::GET, &blob, b"").await;
        let config: ImageConfiguration = serde_json::from_slice(&served).unwrap();
        assert_eq!(config.rootfs().diff_ids(), &vec![layer.to_string()]);

        let (_, headers, again) =
            send(server, Method::GET, "/images/v2/alpine/manifests/3.21", b"").await;
        assert_eq!(header(&headers, "docker-content-digest"), digest);
        assert_eq!(again, body);
    }

    /// Two manifests of the same layers and config are two images, each served under its tag
    /// as it was put.
    #[tokio::test]
    async fn a_tag_serves_the_manifest_it_was_put_as() {
        let scratch = Scratch::new("annotated");
        let server = &scratch.1;
        let layer = erofs(b"same");
        let config = config(&[&layer]);
        for blob in [&layer, &config] {
            let post = format!("/images/v2/app/blobs/uploads/?digest={}", sha256(blob));
            send(server, Method::POST, &post, blob).await;
        }
        let plain = manifest(&config, &[(EROFS_LAYER, &layer)]);
        let mut annotated: serde_json::Value = serde_json::from_slice(&plain).unwrap();
        annotated["annotations"] = json!({"org.opencontainers.image.revision": "abc123"});
        let annotated = serde_json::to_vec(&annotated).unwrap();

        send(server, Method::PUT, "/images/v2/app/manifests/v1", &plain).await;
        send(
            server,
            Method::PUT,
            "/images/v2/app/manifests/v2",
            &annotated,
        )
        .await;
        let (_, _, v1) = send(server, Method::GET, "/images/v2/app/manifests/v1", b"").await;
        let (_, _, v2) = send(server, Method::GET, "/images/v2/app/manifests/v2", b"").await;
        assert_eq!((v1, v2), (plain, annotated));
    }

    /// Removing an image by its digest in one repository leaves its tags in another, and the
    /// image with them.
    #[tokio::test]
    async fn a_removal_by_digest_stays_in_its_repository() {
        let scratch = Scratch::new("remove");
        let (store, server) = (&scratch.0, &scratch.1);
        let layer = store.insert_layer(&FsTree::new()).unwrap();
        let image = store
            .insert_image(&ImageConfiguration::default(), &[layer])
            .unwrap();
        for tag in ["alpine:3.21", "alpine:copy", "team/app:v1"] {
            store.insert_tag(&tag.parse().unwrap(), &image).unwrap();
        }

        let (_, _, body) = send(server, Method::GET, "/images/v2/_catalog", b"").await;
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap()["repositories"],
            json!(["docker.io/library/alpine", "docker.io/team/app"])
        );

        let delete = format!("/images/v2/alpine/manifests/{image}");
        let (status, _, _) = send(server, Method::DELETE, &delete, b"").await;
        assert_eq!(status, StatusCode::ACCEPTED);
        let tags: Vec<String> = store
            .tags()
            .unwrap()
            .iter()
            .map(|(reference, _)| reference.to_string())
            .collect();
        assert_eq!(tags, vec!["docker.io/team/app:v1"]);
        assert!(store.get_image(&image).unwrap().is_some());

        let (status, _, _) = send(server, Method::DELETE, &delete, b"").await;
        assert_eq!(status, StatusCode::NOT_FOUND, "not alpine's any more");
        let delete = format!("/images/v2/team/app/manifests/{image}");
        let (status, _, _) = send(server, Method::DELETE, &delete, b"").await;
        assert_eq!(status, StatusCode::ACCEPTED);
        assert!(store.get_image(&image).unwrap().is_none());
    }

    #[tokio::test]
    async fn what_is_not_an_erofs_image_is_refused() {
        let scratch = Scratch::new("refused");
        let server = &scratch.1;
        let tar = b"not an erofs image".to_vec();
        let post = format!("/images/v2/app/blobs/uploads/?digest={}", sha256(&tar));
        let (status, _, _) = send(server, Method::POST, &post, &tar).await;
        assert_eq!(
            status,
            StatusCode::CREATED,
            "kept as a blob, as a config is"
        );

        let config = config(&[&tar]);
        let post = format!("/images/v2/app/blobs/uploads/?digest={}", sha256(&config));
        send(server, Method::POST, &post, &config).await;
        let manifest = manifest(
            &config,
            &[(MediaType::ImageLayer.to_string().as_str(), &tar)],
        );
        let (status, _, body) = send(
            server,
            Method::PUT,
            "/images/v2/app/manifests/v1",
            &manifest,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(String::from_utf8_lossy(&body).contains("MANIFEST_INVALID"));

        let wrong = format!("/images/v2/app/blobs/uploads/?digest={}", sha256(b"other"));
        let (status, _, body) = send(server, Method::POST, &wrong, &tar).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(String::from_utf8_lossy(&body).contains("DIGEST_INVALID"));
    }
}
