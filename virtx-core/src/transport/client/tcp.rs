//! The client's end of HTTP over TCP: the server at a base URL, each endpoint a path under it.

use std::sync::{Arc, PoisonError, RwLock};

use http::HeaderMap;
use http_body_util::BodyExt;

use super::Body;

/// The headers sent with every request, read as each is sent, so that a change reaches the
/// next one: a forwarder opened before keeps a hold on them too.
pub(super) type Headers = Arc<RwLock<HeaderMap>>;

/// `headers`, as sent now.
pub(super) fn current(headers: &Headers) -> HeaderMap {
    headers
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}

/// The server at a base URL. Nothing to close: the server keeps nothing between requests.
pub(super) struct Tcp {
    pub(super) http: reqwest::Client,
    /// Ends in `/`, so a path joins under it.
    pub(super) base: reqwest::Url,
    pub(super) headers: Headers,
}

impl Tcp {
    /// The server whose base URL is `url`, an `http` or `https` URL, with `headers` sent on
    /// every request to it -- the credentials it wants, such as `Authorization`. Nothing is
    /// sent until the first request.
    pub(super) fn connect(url: &str, headers: HeaderMap) -> anyhow::Result<Tcp> {
        let mut base = reqwest::Url::parse(url)?;
        anyhow::ensure!(
            matches!(base.scheme(), "http" | "https"),
            "not an http or https URL: {base}"
        );
        if !base.path().ends_with('/') {
            base.set_path(&format!("{}/", base.path()));
        }
        Ok(Tcp {
            http: reqwest::Client::new(),
            base,
            headers: Arc::new(RwLock::new(headers)),
        })
    }

    /// Send `headers` with every request from now on, in place of those sent until now.
    pub(super) fn set_headers(&self, headers: HeaderMap) {
        *self.headers.write().unwrap_or_else(PoisonError::into_inner) = headers;
    }

    /// Send `request`, whose URI is a path under the base, such as `/rpc`.
    pub(super) async fn send(
        &self,
        request: http::Request<Body>,
    ) -> Result<http::Response<Body>, String> {
        let (parts, body) = request.into_parts();
        let url = self
            .base
            .join(parts.uri.to_string().trim_start_matches('/'))
            .map_err(|e| format!("{e:#}"))?;
        // The request's own headers win over the ones sent with every request.
        let mut headers = current(&self.headers);
        headers.extend(parts.headers);
        let response = self
            .http
            .request(parts.method, url)
            .headers(headers)
            .body(reqwest::Body::wrap(body))
            .send()
            .await
            .map_err(|e| format!("{e:#}"))?;
        let response: http::Response<reqwest::Body> = response.into();
        Ok(response.map(|body| body.map_err(std::io::Error::other).boxed()))
    }
}
