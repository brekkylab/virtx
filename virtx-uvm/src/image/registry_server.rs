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
use crate::backend::{Backing, TagError, Wanted};
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

#[derive(Clone, Debug)]
pub struct RegistryServer {
    store: Store,
    /// The path the spec's `/v2/` is under, such as `/images`.
    base: String,
    /// Each blob being pushed, until it is whole, and each whole one that is not a layer, until
    /// a manifest names it: in the temp directory, and gone with this.
    uploads: PathBuf,
    /// Where its images are kept beyond this machine, once its home is given that.
    backing: Backing,
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
            backing: Backing::default(),
        })
    }

    /// Where its images are kept beyond this machine, once there is a backend for them: the
    /// home over its store shares this, and backs it as it is backed. Its catalog and tag lists
    /// list the tags kept there too, and a manifest named by its tag or digest is
    /// [wanted](crate::StorageBackend::want) from there before it is answered. A tag pushed is
    /// kept there, and one removed is removed there, before either is answered.
    pub fn backing(&self) -> &Backing {
        &self.backing
    }

    /// The tags kept beyond this machine and not here, for a listing beside the store's: those
    /// with a tag in them. Refused `500` where the backend fails, which is logged, so that no
    /// listing is answered short.
    async fn kept_tags(&self) -> Result<Vec<Reference>> {
        let Some(backend) = self.backing.get() else {
            return Ok(Vec::new());
        };
        match backend.tags().await {
            Ok(tags) => Ok(tags.into_iter().filter(|tag| tag.tag().is_some()).collect()),
            Err(e) => {
                eprintln!("listing the tags kept beyond this machine: {e:#}");
                Err(refusal(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "UNKNOWN",
                    "the tags kept beyond this machine could not be listed",
                ))
            }
        }
    }

    /// The image the manifest `reference` of repository `name` names -- a tag, or a digest --
    /// made ready here from beyond this machine, where it is kept there. Answered from what is
    /// here whatever comes of it.
    async fn want(&self, name: &str, reference: &str) {
        let Some(backend) = self.backing.get() else {
            return;
        };
        let wanted = match reference.parse::<Digest>() {
            Ok(digest) => backend.want(Wanted::Image(&digest)).await,
            Err(_) => match tagged(name, reference) {
                Ok(tag) => backend.want(Wanted::Tag(&tag)).await,
                Err(_) => return,
            },
        };
        if let Err(e) = wanted {
            eprintln!("bringing {name}:{reference} here: {e:#}");
        }
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
                Method::GET => {
                    let also = self.kept_tags().await?;
                    self.catalog(&also, asked("n"), asked("last"))
                }
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
                Method::GET => {
                    let also = self.kept_tags().await?;
                    self.tags(name, &also, asked("n"), asked("last"))
                }
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
            ("manifests", Method::GET) => {
                self.want(name, last).await;
                self.get_manifest(name, last, true).await
            }
            ("manifests", Method::HEAD) => {
                self.want(name, last).await;
                self.get_manifest(name, last, false).await
            }
            ("manifests", Method::PUT) => self.put_manifest(name, last, request.into_body()).await,
            ("manifests", Method::DELETE) => self.delete_manifest(name, last).await,
            ("blobs", Method::GET) => self.get_blob(last, true).await,
            ("blobs", Method::HEAD) => self.get_blob(last, false).await,
            ("blobs", Method::DELETE) => {
                let (server, last) = (self.clone(), last.to_owned());
                tokio::task::spawn_blocking(move || server.delete_blob(&last)).await?
            }
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

    /// Every repository a tag is in, here or in `also`, as the spec pages a tag listing.
    fn catalog(
        &self,
        also: &[Reference],
        n: Option<&str>,
        last: Option<&str>,
    ) -> Result<Response<Body>> {
        let mut repositories: Vec<String> = (self.store.tags()?.iter())
            .map(|(reference, _)| reference)
            .chain(also)
            .map(repository)
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

    /// The tags of repository `name`, here or in `also`, as the spec pages them.
    fn tags(
        &self,
        name: &str,
        also: &[Reference],
        n: Option<&str>,
        last: Option<&str>,
    ) -> Result<Response<Body>> {
        let wanted = repository(&tagged(name, "latest")?);
        let mut tags: Vec<String> = (self.store.tags()?.iter())
            .map(|(reference, _)| reference)
            .chain(also)
            .filter(|reference| repository(reference) == wanted)
            .filter_map(|reference| reference.tag().map(str::to_owned))
            .collect();
        if tags.is_empty() {
            return Err(refusal(StatusCode::NOT_FOUND, "NAME_UNKNOWN", name));
        }
        tags.sort();
        tags.dedup();
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
        // Filing waits for the store's change under way: blocking work.
        let (store, uploads) = (self.store.clone(), self.uploads.clone());
        let config_digest = manifest.config().digest().digest().to_owned();
        tokio::task::spawn_blocking(move || -> Result<()> {
            store
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
            let _ = std::fs::remove_file(uploads.join(config_digest));
            Ok(())
        })
        .await??;
        if let Some(tag) = tag {
            match self.backing.tag(&self.store, &tag, &digest).await {
                Ok(()) => {}
                Err(TagError::Store(e)) => return Err(e.into()),
                Err(TagError::Kept(e)) => {
                    eprintln!("keeping {tag} beyond this machine: {e:#}");
                    return Err(refusal(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "UNKNOWN",
                        "the tag could not be kept beyond this machine",
                    ));
                }
            }
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
    /// repository, and the image too once no tag in any other names it. Either is refused
    /// `409 DENIED`, with nothing removed, where it would leave an image a sandbox or a build
    /// stands on named by no tag. Each tag taken away here is then removed where it is kept
    /// ([`untag`](crate::StorageBackend::untag)), and a tag not here is removed there alone, its
    /// image not brought here for it; answered once that is done, and run to its end even if
    /// the request is dropped. A backend's failure is refused `500`, each tag it has not
    /// removed put back here. Each tag it takes away is held from its removal here to the
    /// backend's ([`Backing::lock_tags`]), so that no push of it comes between; one that names
    /// the digest's image only once that is held is left, as pushed after.
    async fn delete_manifest(&self, name: &str, reference: &str) -> Result<Response<Body>> {
        let (store, backing) = (self.store.clone(), self.backing.clone());
        let (name, reference) = (name.to_owned(), reference.to_owned());
        tokio::spawn(async move {
            // A removal waits for the store's change under way: blocking work.
            let named = {
                let (store, name, reference) = (store.clone(), name.clone(), reference.clone());
                tokio::task::spawn_blocking(move || named_by(&store, &name, &reference)).await??
            };
            let locked = backing.lock_tags(named).await;
            let taken = {
                let (store, name, reference) = (store.clone(), name.clone(), reference.clone());
                tokio::task::spawn_blocking(move || {
                    delete_here(&store, &name, &reference, |tag| locked.holds(tag))
                        .map(|taken| (taken, locked))
                })
                .await??
            };
            let (taken, _locked) = taken;
            let mut kept = false;
            if let Some(backend) = backing.get() {
                for (at, (tag, _)) in taken.tags.iter().enumerate() {
                    match backend.untag(tag).await {
                        Ok(was) => kept |= was,
                        Err(e) => {
                            eprintln!("removing {tag} where it is kept: {e:#}");
                            let back = taken.tags[at..].to_vec();
                            tokio::task::spawn_blocking(move || {
                                for (tag, image) in back {
                                    if image.is_some()
                                        && let Err(e) = store.restore_tag(&tag, image.as_ref())
                                    {
                                        eprintln!("putting {tag} back here: {e}");
                                    }
                                }
                            })
                            .await?;
                            return Err(refusal(
                                StatusCode::INTERNAL_SERVER_ERROR,
                                "UNKNOWN",
                                "the tag could not be removed where it is kept",
                            ));
                        }
                    }
                }
            }
            if let Some(image) = taken.image {
                tokio::task::spawn_blocking(move || match store.remove_image(&image) {
                    Ok(_) | Err(store::Error::InUse(..)) => Ok(()),
                    Err(e) => Err(e),
                })
                .await??;
            }
            match taken.here || kept {
                true => Ok(empty(StatusCode::ACCEPTED)),
                false => Err(refusal(
                    StatusCode::NOT_FOUND,
                    "MANIFEST_UNKNOWN",
                    reference,
                )),
            }
        })
        .await?
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

/// What a manifest's `DELETE` took away here.
struct Taken {
    /// Whether it named something here.
    here: bool,
    /// Each tag taken away, with the image it named here: none for a tag not here.
    tags: Vec<(Reference, Option<Digest>)>,
    /// The image a digest named, to be removed once its tags are removed where they are kept,
    /// unless a tag names it still or something holds it by then.
    image: Option<Digest>,
}

/// The tags a manifest's `DELETE` of `reference` in repository `name` would take away in
/// `store` now: the tag it names, or each tag in `name` naming the digest's image. Unchecked:
/// [`delete_here`] answers for what is wrong with it.
fn named_by(store: &Store, name: &str, reference: &str) -> Result<Vec<Reference>> {
    if !reference.contains(':') {
        return Ok(tagged(name, reference).into_iter().collect());
    }
    let (Ok(digest), Ok(here)) = (reference.parse::<Digest>(), tagged(name, "latest")) else {
        return Ok(Vec::new());
    };
    let here = repository(&here);
    Ok((store.tags()?.into_iter())
        .filter(|(tag, named)| *named == digest && repository(tag) == here)
        .map(|(tag, _)| tag)
        .collect())
}

/// What a manifest's `DELETE` of `reference` in repository `name` takes away in `store`, as
/// [`RegistryServer::delete_manifest`] says: of a digest's tags, only those `held` picks.
fn delete_here(
    store: &Store,
    name: &str,
    reference: &str,
    held: impl Fn(&Reference) -> bool,
) -> Result<Taken> {
    let unknown = || refusal(StatusCode::NOT_FOUND, "MANIFEST_UNKNOWN", reference);
    let in_use = |image: &Digest| {
        refusal(
            StatusCode::CONFLICT,
            "DENIED",
            format!("in use: a sandbox or a build stands on {image}, and no other tag names it"),
        )
    };
    if !reference.contains(':') {
        let tag = tagged(name, reference)?;
        return match store.remove_tag(&tag) {
            Ok(image) => Ok(Taken {
                here: image.is_some(),
                tags: vec![(tag, image)],
                image: None,
            }),
            Err(store::Error::InUse(image, _)) => Err(in_use(&image)),
            Err(e) => Err(e.into()),
        };
    }
    let digest = reference
        .parse::<Digest>()
        .map_err(|e| refusal(StatusCode::BAD_REQUEST, "DIGEST_INVALID", e.to_string()))?;
    if store.get_image_manifest(&digest)?.is_none() {
        return Err(unknown());
    }
    let here = repository(&tagged(name, "latest")?);
    let (tags_here, tags_elsewhere): (Vec<_>, Vec<_>) = store
        .tags()?
        .into_iter()
        .filter(|(_, named)| *named == digest)
        .partition(|(tag, _)| repository(tag) == here);
    // An image tagged only in other repositories is not this one's.
    if tags_here.is_empty() && !tags_elsewhere.is_empty() {
        return Err(unknown());
    }
    // A tag pushed since the tags to take away were held is left, as pushed after.
    if !tags_here.is_empty() && !tags_here.iter().any(|(tag, _)| held(tag)) {
        return Ok(Taken {
            here: true,
            tags: Vec::new(),
            image: None,
        });
    }
    let taken = match tags_here.is_empty() {
        // Named by no tag: removed now, as there is none to remove anywhere else first.
        true => store.remove_image(&digest).map(|_| Vec::new()),
        false => store.remove_tags_of(&digest, |tag| repository(tag) == here && held(tag)),
    };
    match taken {
        Ok(tags) => Ok(Taken {
            here: true,
            image: (!tags.is_empty()).then(|| digest.clone()),
            tags: (tags.into_iter())
                .map(|tag| (tag, Some(digest.clone())))
                .collect(),
        }),
        Err(store::Error::InUse(image, _)) => Err(in_use(&image)),
        Err(e) => Err(e.into()),
    }
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
    use std::collections::HashMap;

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

    /// An image a sandbox stands on is not left named by no tag: its last tag, or its digest,
    /// is refused `409 DENIED` with every tag kept; another tag of it may go. Let go, it may be
    /// removed.
    #[tokio::test]
    async fn an_image_a_sandbox_stands_on_keeps_its_last_tag() {
        let scratch = Scratch::new("held");
        let (store, server) = (&scratch.0, &scratch.1);
        let layer = store.insert_layer(&FsTree::new()).unwrap();
        let image = store
            .insert_image(&ImageConfiguration::default(), &[layer])
            .unwrap();
        for tag in ["alpine:3.21", "alpine:copy", "alpine:other"] {
            store.insert_tag(&tag.parse().unwrap(), &image).unwrap();
        }
        let hold = store.hold_image(&image).unwrap();
        let (status, _, _) = send(
            server,
            Method::DELETE,
            "/images/v2/alpine/manifests/copy",
            b"",
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED, "another tag names it still");

        let by_digest = format!("/images/v2/alpine/manifests/{image}");
        let (status, _, body) = send(server, Method::DELETE, &by_digest, b"").await;
        assert_eq!(status, StatusCode::CONFLICT);
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["errors"][0]["code"], "DENIED", "{body}");
        assert_eq!(store.tags().unwrap().len(), 2, "its tags kept");

        let (status, _, _) = send(
            server,
            Method::DELETE,
            "/images/v2/alpine/manifests/other",
            b"",
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED);
        let (status, _, body) = send(
            server,
            Method::DELETE,
            "/images/v2/alpine/manifests/3.21",
            b"",
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["errors"][0]["code"], "DENIED", "{body}");
        assert_eq!(store.tags().unwrap().len(), 1, "its last tag kept");

        drop(hold);
        let (status, _, _) = send(server, Method::DELETE, &by_digest, b"").await;
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

    /// An image kept beyond this machine, tagged `kept:v1`, brought here as a backend would
    /// bring it: by its tag or its digest. It lists a reference by digest alone too. A tag
    /// told to it is said in `tagged`, with its image, and kept from then on; one removed, in
    /// `untagged`. It fails each of those while `failing`, taking the image of a tag it fails
    /// to keep where `taking`, as a sandbox made of it meanwhile would. Where `paused`, a
    /// removal waits for a permit of it, `untagging` set as it does. What it keeps is `kept`.
    #[derive(Default)]
    struct Elsewhere {
        here: Option<Store>,
        wanted: std::sync::Mutex<Vec<String>>,
        tagged: std::sync::Mutex<Vec<(String, Digest)>>,
        untagged: std::sync::Mutex<Vec<String>>,
        kept: std::sync::Mutex<HashMap<String, Digest>>,
        failing: bool,
        taking: bool,
        taken: std::sync::Mutex<Vec<store::ImageHold>>,
        paused: Option<tokio::sync::Semaphore>,
        untagging: std::sync::atomic::AtomicBool,
    }

    impl Elsewhere {
        fn over(here: &Store) -> Elsewhere {
            Elsewhere {
                here: Some(here.clone()),
                ..Elsewhere::default()
            }
        }

        fn failing(here: &Store) -> Elsewhere {
            Elsewhere {
                failing: true,
                ..Elsewhere::over(here)
            }
        }
    }

    /// File the image [`Elsewhere`] keeps in `store`: the same digest in every store.
    fn kept_image(store: &Store) -> Digest {
        let layer = store.insert_layer(&FsTree::new()).unwrap();
        let config = ImageConfiguration::default();
        store.insert_image(&config, &[layer]).unwrap()
    }

    impl crate::StorageBackend for Elsewhere {
        fn want<'a>(
            &'a self,
            wanted: Wanted<'a>,
        ) -> futures_util::future::BoxFuture<'a, anyhow::Result<()>> {
            Box::pin(async move {
                let elsewhere = Scratch::new("elsewhere");
                let kept = kept_image(&elsewhere.0);
                let here = self.here.as_ref().unwrap();
                let said = match wanted {
                    Wanted::Tag(tag) => {
                        if *tag == "kept:v1".parse::<Reference>().unwrap() {
                            here.insert_tag(tag, &kept_image(here))?;
                        }
                        tag.to_string()
                    }
                    Wanted::Image(digest) => {
                        if *digest == kept {
                            kept_image(here);
                        }
                        digest.to_string()
                    }
                };
                self.wanted.lock().unwrap().push(said);
                Ok(())
            })
        }

        fn tags(&self) -> futures_util::future::BoxFuture<'_, anyhow::Result<Vec<Reference>>> {
            Box::pin(async {
                anyhow::ensure!(!self.failing, "the backend is down");
                let untagged = format!("untagged@sha256:{}", "0".repeat(64));
                Ok(vec!["kept:v1".parse().unwrap(), untagged.parse().unwrap()])
            })
        }

        fn tagged<'a>(
            &'a self,
            tag: &'a Reference,
            image: &'a Digest,
        ) -> futures_util::future::BoxFuture<'a, anyhow::Result<()>> {
            Box::pin(async move {
                if self.failing && self.taking {
                    let held = self.here.as_ref().unwrap().hold_image(image)?;
                    self.taken.lock().unwrap().push(held);
                }
                anyhow::ensure!(!self.failing, "the backend is down");
                let told = (tag.to_string(), image.clone());
                self.tagged.lock().unwrap().push(told.clone());
                self.kept.lock().unwrap().insert(told.0, told.1);
                Ok(())
            })
        }

        fn untag<'a>(
            &'a self,
            tag: &'a Reference,
        ) -> futures_util::future::BoxFuture<'a, anyhow::Result<bool>> {
            Box::pin(async move {
                if let Some(paused) = &self.paused {
                    (self.untagging).store(true, std::sync::atomic::Ordering::SeqCst);
                    paused.acquire().await?.forget();
                }
                anyhow::ensure!(!self.failing, "the backend is down");
                let tag = tag.to_string();
                let kept = tag == "docker.io/library/kept:v1"
                    || (self.tagged.lock().unwrap().iter()).any(|(told, _)| *told == tag);
                self.kept.lock().unwrap().remove(&tag);
                self.untagged.lock().unwrap().push(tag);
                Ok(kept)
            })
        }
    }

    /// A registry whose images are kept beyond this machine lists the tags kept there beside its
    /// own, and brings a manifest named by its tag or its digest here before it answers it.
    #[tokio::test]
    async fn a_backed_registry_lists_what_is_kept_and_brings_what_is_asked_here() {
        for by_digest in [false, true] {
            let scratch = Scratch::new(&format!("backed-{by_digest}"));
            let (store, server) = (&scratch.0, &scratch.1);
            let elsewhere = std::sync::Arc::new(Elsewhere::over(store));
            assert!(server.backing().set(elsewhere.clone()));
            let digest = kept_image(&Scratch::new("kept-digest").0).to_string();

            let (_, _, body) = send(server, Method::GET, "/images/v2/_catalog", b"").await;
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&body).unwrap()["repositories"],
                json!(["docker.io/library/kept"])
            );
            let (status, _, body) =
                send(server, Method::GET, "/images/v2/kept/tags/list", b"").await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&body).unwrap()["tags"],
                json!(["v1"])
            );
            assert!(store.images().unwrap().is_empty(), "listed, not brought");

            let named = match by_digest {
                true => digest.clone(),
                false => "v1".to_owned(),
            };
            let manifest = format!("/images/v2/kept/manifests/{named}");
            let (status, headers, _) = send(server, Method::HEAD, &manifest, b"").await;
            assert_eq!(status, StatusCode::OK, "{named}");
            assert_eq!(header(&headers, "docker-content-digest"), digest);
            let wanted = match by_digest {
                true => digest.clone(),
                false => "docker.io/library/kept:v1".to_owned(),
            };
            assert_eq!(*elsewhere.wanted.lock().unwrap(), [wanted]);
        }
        let scratch = Scratch::new("backed-none");
        assert!((scratch.1.backing()).set(std::sync::Arc::new(Elsewhere::over(&scratch.0))));
        let none = "/images/v2/none/manifests/v1";
        let (status, _, _) = send(&scratch.1, Method::GET, none, b"").await;
        assert_eq!(status, StatusCode::NOT_FOUND, "kept nowhere");
        let untagged = "/images/v2/untagged/tags/list";
        let (status, _, _) = send(&scratch.1, Method::GET, untagged, b"").await;
        assert_eq!(status, StatusCode::NOT_FOUND, "no tag of it is kept");
    }

    /// A registry whose backend cannot list what it keeps refuses its listings `500`, rather
    /// than answering them with the tags here alone.
    #[tokio::test]
    async fn a_backed_registry_refuses_a_listing_its_backend_cannot_give() {
        let scratch = Scratch::new("backed-down");
        let (store, server) = (&scratch.0, &scratch.1);
        store
            .insert_tag(&"here:v1".parse().unwrap(), &kept_image(store))
            .unwrap();
        assert!((server.backing()).set(std::sync::Arc::new(Elsewhere::failing(store))));
        for listing in ["/images/v2/_catalog", "/images/v2/here/tags/list"] {
            let (status, _, _) = send(server, Method::GET, listing, b"").await;
            assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{listing}");
        }
    }

    /// Push `layer` as image `tag` of repository `name`, by its tag or, with `None`, its
    /// digest: what the push answers, and the image's digest.
    async fn push(
        server: &RegistryServer,
        name: &str,
        tag: Option<&str>,
        layer: &[u8],
    ) -> (StatusCode, Digest) {
        let config = config(&[layer]);
        for blob in [layer, &config] {
            let post = format!("/images/v2/{name}/blobs/uploads/?digest={}", sha256(blob));
            send(server, Method::POST, &post, blob).await;
        }
        let manifest = manifest(&config, &[(EROFS_LAYER, layer)]);
        let digest = sha256(&manifest);
        let named = tag.map_or(digest.to_string(), str::to_owned);
        let put = format!("/images/v2/{name}/manifests/{named}");
        let (status, _, _) = send(server, Method::PUT, &put, &manifest).await;
        (status, digest)
    }

    /// A tag pushed to a registry whose images are kept beyond this machine is told to be kept
    /// there, as naming the image pushed, before the push is answered; a manifest pushed by its
    /// digest alone tells it nothing.
    #[tokio::test]
    async fn a_tag_pushed_to_a_backed_registry_is_kept_where_its_images_are() {
        let scratch = Scratch::new("backed-push");
        let (store, server) = (&scratch.0, &scratch.1);
        let elsewhere = std::sync::Arc::new(Elsewhere::over(store));
        assert!(server.backing().set(elsewhere.clone()));

        let (status, image) = push(server, "app", Some("v1"), &erofs(b"one")).await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(
            *elsewhere.tagged.lock().unwrap(),
            [("docker.io/library/app:v1".to_owned(), image.clone())]
        );
        assert_eq!(
            store.get_tag(&"app:v1".parse().unwrap()).unwrap(),
            Some(image)
        );

        let (status, _) = push(server, "app", None, &erofs(b"two")).await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(elsewhere.tagged.lock().unwrap().len(), 1, "no tag to keep");
    }

    /// A push whose tag its backend cannot keep is refused `500`, and leaves the tag here as it
    /// was: naming the image it named, or none.
    #[tokio::test]
    async fn a_tag_its_backend_cannot_keep_is_left_as_it_was() {
        let scratch = Scratch::new("backed-push-down");
        let (store, server) = (&scratch.0, &scratch.1);
        let before = kept_image(store);
        let v1: Reference = "app:v1".parse().unwrap();
        store.insert_tag(&v1, &before).unwrap();
        assert!(
            server
                .backing()
                .set(std::sync::Arc::new(Elsewhere::failing(store)))
        );

        let (status, _) = push(server, "app", Some("v1"), &erofs(b"one")).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(store.get_tag(&v1).unwrap(), Some(before), "named as before");

        let (status, _) = push(server, "app", Some("v2"), &erofs(b"two")).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(store.get_tag(&"app:v2".parse().unwrap()).unwrap(), None);

        // A new tag whose image a sandbox took meanwhile goes too, though it was the image's last.
        let scratch = Scratch::new("backed-push-down-taken");
        let (store, server) = (&scratch.0, &scratch.1);
        let elsewhere = std::sync::Arc::new(Elsewhere {
            taking: true,
            ..Elsewhere::failing(store)
        });
        assert!(server.backing().set(elsewhere.clone()));
        let (status, image) = push(server, "app", Some("v3"), &erofs(b"three")).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(elsewhere.taken.lock().unwrap().len(), 1, "taken");
        assert_eq!(store.get_tag(&"app:v3".parse().unwrap()).unwrap(), None);
        assert!(
            store.get_image(&image).unwrap().is_some(),
            "the image stays"
        );
    }

    /// A push of a tag whose `DELETE` is under way where the tag is kept waits for it, so that
    /// the tag ends here and there naming what was pushed: the push comes after the removal in
    /// both.
    #[tokio::test]
    async fn a_push_of_a_tag_being_removed_comes_after_its_removal() {
        let scratch = Scratch::new("backed-push-untag");
        let (store, server) = (&scratch.0, &scratch.1);
        let elsewhere = std::sync::Arc::new(Elsewhere {
            paused: Some(tokio::sync::Semaphore::new(0)),
            ..Elsewhere::over(store)
        });
        assert!(server.backing().set(elsewhere.clone()));
        let (status, _) = push(server, "app", Some("v1"), &erofs(b"one")).await;
        assert_eq!(status, StatusCode::CREATED);

        let untagging = async || {
            while !(elsewhere.untagging).load(std::sync::atomic::Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        let delete = send(server, Method::DELETE, "/images/v2/app/manifests/v1", b"");
        let pushed = async {
            untagging().await;
            push(server, "app", Some("v1"), &erofs(b"two")).await
        };
        let released = async {
            untagging().await;
            // Time for a push not ordered after the removal to land first.
            tokio::time::sleep(Duration::from_millis(300)).await;
            elsewhere.paused.as_ref().unwrap().add_permits(1);
        };
        let ((deleted, _, _), (pushed, image), ()) = tokio::join!(delete, pushed, released);
        assert_eq!(
            (deleted, pushed),
            (StatusCode::ACCEPTED, StatusCode::CREATED)
        );
        let v1: Reference = "app:v1".parse().unwrap();
        assert_eq!(store.get_tag(&v1).unwrap().as_ref(), Some(&image), "here");
        assert_eq!(
            elsewhere
                .kept
                .lock()
                .unwrap()
                .get("docker.io/library/app:v1"),
            Some(&image),
            "where it is kept"
        );
    }

    /// A tag's `DELETE` removes it here and where it is kept, answered once both are; one kept
    /// and not here is removed there alone, its image not brought here first, so that one
    /// whose image cannot be still goes; one kept nowhere is unknown. A tag here that the store
    /// refuses to take away is left where it is kept too.
    #[tokio::test]
    async fn a_backed_registry_removes_a_kept_tag_without_bringing_its_image_here() {
        let scratch = Scratch::new("backed-untag");
        let (store, server) = (&scratch.0, &scratch.1);
        let elsewhere = std::sync::Arc::new(Elsewhere::over(store));
        assert!(server.backing().set(elsewhere.clone()));

        let here: Reference = "here:v1".parse().unwrap();
        store.insert_tag(&here, &kept_image(store)).unwrap();
        let (status, _, _) =
            send(server, Method::DELETE, "/images/v2/here/manifests/v1", b"").await;
        assert_eq!(status, StatusCode::ACCEPTED, "here, if not kept");
        assert!(store.get_tag(&here).unwrap().is_none());
        assert_eq!(
            *elsewhere.untagged.lock().unwrap(),
            ["docker.io/library/here:v1"]
        );
        let image = kept_image(store);
        store.remove_image(&image).unwrap();

        let kept = "/images/v2/kept/manifests/v1";
        let (status, _, _) = send(server, Method::DELETE, kept, b"").await;
        assert_eq!(status, StatusCode::ACCEPTED, "kept, if not here");
        assert_eq!(
            elsewhere.untagged.lock().unwrap()[1],
            "docker.io/library/kept:v1"
        );
        assert!(
            elsewhere.wanted.lock().unwrap().is_empty(),
            "not brought here"
        );
        assert!(store.images().unwrap().is_empty());

        let none = "/images/v2/none/manifests/v1";
        let (status, _, _) = send(server, Method::DELETE, none, b"").await;
        assert_eq!(status, StatusCode::NOT_FOUND, "kept nowhere");

        let held: Reference = "held:v1".parse().unwrap();
        let image = kept_image(store);
        store.insert_tag(&held, &image).unwrap();
        let _hold = store.hold_image(&image).unwrap();
        let (status, _, _) =
            send(server, Method::DELETE, "/images/v2/held/manifests/v1", b"").await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(store.get_tag(&held).unwrap(), Some(image));
        assert_eq!(
            elsewhere.untagged.lock().unwrap().len(),
            3,
            "not removed there"
        );
    }

    /// A digest's `DELETE` removes each tag it takes away here from where it is kept too, and
    /// no tag of another repository.
    #[tokio::test]
    async fn a_digest_deleted_from_a_backed_registry_removes_its_tags_where_they_are_kept() {
        let scratch = Scratch::new("backed-untag-digest");
        let (store, server) = (&scratch.0, &scratch.1);
        let elsewhere = std::sync::Arc::new(Elsewhere::over(store));
        assert!(server.backing().set(elsewhere.clone()));
        let image = kept_image(store);
        for tag in ["app:v1", "app:v2", "other:v1"] {
            store.insert_tag(&tag.parse().unwrap(), &image).unwrap();
        }

        let digest = format!("/images/v2/app/manifests/{image}");
        let (status, _, _) = send(server, Method::DELETE, &digest, b"").await;
        assert_eq!(status, StatusCode::ACCEPTED);
        assert_eq!(
            *elsewhere.untagged.lock().unwrap(),
            ["docker.io/library/app:v1", "docker.io/library/app:v2"]
        );
        assert_eq!(store.tags().unwrap().len(), 1, "other's kept");
    }

    /// A `DELETE` its backend cannot carry out where the tag is kept is refused `500`, and
    /// leaves the tag here.
    #[tokio::test]
    async fn a_tag_its_backend_cannot_remove_is_left_here() {
        let scratch = Scratch::new("backed-untag-down");
        let (store, server) = (&scratch.0, &scratch.1);
        let image = kept_image(store);
        let here: Reference = "here:v1".parse().unwrap();
        store.insert_tag(&here, &image).unwrap();
        assert!(
            server
                .backing()
                .set(std::sync::Arc::new(Elsewhere::failing(store)))
        );

        for removed in ["v1".to_owned(), image.to_string()] {
            let delete = format!("/images/v2/here/manifests/{removed}");
            let (status, _, _) = send(server, Method::DELETE, &delete, b"").await;
            assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{removed}");
            assert_eq!(
                store.get_tag(&here).unwrap(),
                Some(image.clone()),
                "{removed}"
            );
        }
    }
}
