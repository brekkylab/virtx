//! The registry's requests, one function each, as the OCI Distribution Specification names
//! them under `/images/v2/`.

use bytes::Bytes;
use http::{Method, Response, StatusCode, header};
use http_body_util::BodyExt;
use serde::Deserialize;

use super::manifest::{MANIFEST_TYPES, Manifest};
use crate::{
    Error,
    transport::{Body, TransportClient, full},
};

/// What `Docker-Content-Digest` names: the digest of what was answered or stored.
const CONTENT_DIGEST: &str = "docker-content-digest";

/// How many names a page of a listing asks for.
const PAGE: usize = 1000;

/// The server's registry, over a client's connection.
pub(crate) struct Registry<'a> {
    client: &'a TransportClient,
}

impl<'a> Registry<'a> {
    pub(crate) fn new(client: &'a TransportClient) -> Registry<'a> {
        Registry { client }
    }

    /// Send `method` to `path`, under `images/v2/`, and answer its response if it is a success.
    async fn send(
        &self,
        method: Method,
        path: &str,
        headers: &[(header::HeaderName, String)],
        body: Body,
    ) -> Result<Response<Body>, Error> {
        let mut request = http::Request::builder()
            .method(method.clone())
            .uri(format!("/images/v2/{path}"));
        for (name, value) in headers {
            request = request.header(name, value);
        }
        let request = request
            .body(body)
            .map_err(|e| Error::Registry(format!("{method} {path}: {e}")))?;
        let response = self.client.send(request).await?;
        if response.status().is_success() {
            return Ok(response);
        }
        Err(Error::Registry(format!(
            "{method} {path}: {}",
            failure(response).await
        )))
    }

    /// Every repository, in the order the registry lists them.
    pub(crate) async fn repositories(&self) -> Result<Vec<String>, Error> {
        #[derive(Deserialize)]
        struct Catalog {
            #[serde(default)]
            repositories: Option<Vec<String>>,
        }
        self.pages("_catalog", |body| {
            serde_json::from_slice::<Catalog>(body)
                .map(|page| page.repositories.unwrap_or_default())
        })
        .await
    }

    /// Every tag in `name`.
    pub(crate) async fn tags(&self, name: &str) -> Result<Vec<String>, Error> {
        #[derive(Deserialize)]
        struct Tags {
            #[serde(default)]
            tags: Option<Vec<String>>,
        }
        self.pages(&format!("{name}/tags/list"), |body| {
            serde_json::from_slice::<Tags>(body).map(|page| page.tags.unwrap_or_default())
        })
        .await
    }

    /// Each page of the listing at `path`, read by `read`, until one comes without a `Link` to
    /// the next.
    async fn pages(
        &self,
        path: &str,
        read: impl Fn(&[u8]) -> serde_json::Result<Vec<String>>,
    ) -> Result<Vec<String>, Error> {
        let mut all: Vec<String> = Vec::new();
        loop {
            let mut query = format!("{path}?n={PAGE}");
            if let Some(last) = all.last() {
                query.push_str(&format!("&last={last}"));
            }
            let response = self
                .send(Method::GET, &query, &[], full(Bytes::new()))
                .await?;
            let more = response.headers().contains_key(header::LINK);
            let page = read(&collect(response).await?)
                .map_err(|e| Error::Registry(format!("GET {path}: {e}")))?;
            let ended = page.is_empty();
            all.extend(page);
            if !more || ended {
                return Ok(all);
            }
        }
    }

    /// The digest of the manifest `target` (a tag or digest) in `name`.
    pub(crate) async fn manifest_digest(&self, name: &str, target: &str) -> Result<String, Error> {
        let path = format!("{name}/manifests/{target}");
        let accept = [(header::ACCEPT, MANIFEST_TYPES.to_owned())];
        let response = self
            .send(Method::HEAD, &path, &accept, full(Bytes::new()))
            .await?;
        match header_value(&response, CONTENT_DIGEST) {
            Some(digest) => Ok(digest),
            None => Ok(self.manifest(name, target).await?.digest),
        }
    }

    /// The manifest `target` (a tag or digest) in `name`, checked against `target` if it is a
    /// digest.
    pub(crate) async fn manifest(&self, name: &str, target: &str) -> Result<Manifest, Error> {
        let path = format!("{name}/manifests/{target}");
        let accept = [(header::ACCEPT, MANIFEST_TYPES.to_owned())];
        let response = self
            .send(Method::GET, &path, &accept, full(Bytes::new()))
            .await?;
        let media_type = header_value(&response, header::CONTENT_TYPE.as_str())
            .map(|value| value.split(';').next().unwrap_or("").trim().to_owned());
        let expected = crate::image::is_digest(target).then_some(target);
        Manifest::new(collect(response).await?, media_type, expected)
    }

    /// Store `manifest` in `name` under `target`, a tag or its digest.
    pub(crate) async fn put_manifest(
        &self,
        name: &str,
        target: &str,
        manifest: &Manifest,
    ) -> Result<(), Error> {
        let path = format!("{name}/manifests/{target}");
        let content_type = [(header::CONTENT_TYPE, manifest.media_type.clone())];
        self.send(
            Method::PUT,
            &path,
            &content_type,
            full(manifest.bytes.clone()),
        )
        .await?;
        Ok(())
    }

    /// Delete the manifest `target` in `name`: a tag alone, or by its digest the manifest and
    /// every tag on it.
    pub(crate) async fn delete_manifest(&self, name: &str, target: &str) -> Result<(), Error> {
        let path = format!("{name}/manifests/{target}");
        self.send(Method::DELETE, &path, &[], full(Bytes::new()))
            .await?;
        Ok(())
    }

    /// Whether `name` has the blob `digest`.
    pub(crate) async fn has_blob(&self, name: &str, digest: &str) -> Result<bool, Error> {
        let request = http::Request::head(format!("/images/v2/{name}/blobs/{digest}"))
            .body(full(Bytes::new()))
            .expect("a valid request");
        let response = self.client.send(request).await?;
        match response.status() {
            status if status.is_success() => Ok(true),
            StatusCode::NOT_FOUND => Ok(false),
            _ => Err(Error::Registry(format!(
                "HEAD {name}/blobs/{digest}: {}",
                failure(response).await
            ))),
        }
    }

    /// The blob `digest` in `name`, as it is read.
    pub(crate) async fn blob(&self, name: &str, digest: &str) -> Result<Body, Error> {
        let path = format!("{name}/blobs/{digest}");
        let response = self
            .send(Method::GET, &path, &[], full(Bytes::new()))
            .await?;
        Ok(response.into_body())
    }

    /// Give `name` the blob `digest` that `from` has, without sending it. If the registry does
    /// not mount it, the upload it started instead is answered, to be finished with the blob.
    pub(crate) async fn mount_blob(
        &self,
        name: &str,
        digest: &str,
        from: &str,
    ) -> Result<Option<Upload>, Error> {
        let path = format!("{name}/blobs/uploads/?mount={digest}&from={from}");
        let response = self
            .send(Method::POST, &path, &[], full(Bytes::new()))
            .await?;
        if response.status() == StatusCode::CREATED {
            return Ok(None);
        }
        Upload::from(&response, name).map(Some)
    }

    /// Start uploading a blob to `name`.
    pub(crate) async fn start_upload(&self, name: &str) -> Result<Upload, Error> {
        let path = format!("{name}/blobs/uploads/");
        let response = self
            .send(Method::POST, &path, &[], full(Bytes::new()))
            .await?;
        Upload::from(&response, name)
    }

    /// Finish `upload` with the whole blob, `size` bytes of `body` whose digest is `digest`.
    pub(crate) async fn finish_upload(
        &self,
        upload: Upload,
        digest: &str,
        size: u64,
        body: Body,
    ) -> Result<(), Error> {
        let separator = if upload.path.contains('?') { '&' } else { '?' };
        let path = format!("{}{separator}digest={digest}", upload.path);
        let headers = [
            (header::CONTENT_TYPE, "application/octet-stream".to_owned()),
            (header::CONTENT_LENGTH, size.to_string()),
        ];
        self.send(Method::PUT, &path, &headers, body).await?;
        Ok(())
    }
}

/// An upload a registry started: where to send the blob, under `images/v2/`.
pub(crate) struct Upload {
    path: String,
}

impl Upload {
    /// The upload `response` says it started in `name`, at its `Location`.
    fn from(response: &Response<Body>, name: &str) -> Result<Upload, Error> {
        let location = header_value(response, header::LOCATION.as_str()).ok_or_else(|| {
            Error::Registry(format!("an upload to {name} was started with no Location"))
        })?;
        // A `Location` is a URL, an absolute path or a relative one: what follows `/v2/` is
        // the same in each.
        let path = location
            .split_once("/v2/")
            .map(|(_, path)| path)
            .ok_or_else(|| Error::Registry(format!("an upload at {location:?}, not in /v2/")))?;
        Ok(Upload {
            path: path.to_owned(),
        })
    }
}

fn header_value(response: &Response<Body>, name: &str) -> Option<String> {
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

async fn collect(response: Response<Body>) -> Result<Bytes, Error> {
    response
        .into_body()
        .collect()
        .await
        .map(|body| body.to_bytes())
        .map_err(|e| Error::Transport(format!("reading the response: {e}")))
}

/// Why `response` failed: its status, and the codes and messages of its `{"errors": [...]}`.
async fn failure(response: Response<Body>) -> String {
    #[derive(Deserialize)]
    struct Errors {
        errors: Vec<ErrorItem>,
    }
    #[derive(Deserialize)]
    struct ErrorItem {
        code: String,
        #[serde(default)]
        message: String,
    }
    let status = response.status();
    let body = collect(response).await.unwrap_or_default();
    match serde_json::from_slice::<Errors>(&body) {
        Ok(Errors { errors }) if !errors.is_empty() => {
            let errors: Vec<_> = errors
                .iter()
                .map(|e| format!("{} {}", e.code, e.message).trim().to_owned())
                .collect();
            format!("{status}: {}", errors.join("; "))
        }
        _ => format!("{status}"),
    }
}
