//! The client's end of HTTP over TCP: the server at a base URL, each endpoint a path under it.

use bytes::Bytes;
use http_body_util::BodyExt;

use super::Body;

/// The server at a base URL. Nothing to close: the server keeps nothing between requests.
pub(super) struct Tcp {
    pub(super) http: reqwest::Client,
    /// Ends in `/`, so a path joins under it.
    pub(super) base: reqwest::Url,
}

impl Tcp {
    /// The server whose base URL is `url`, an `http` or `https` URL. Nothing is sent until the
    /// first request.
    pub(super) fn connect(url: &str) -> anyhow::Result<Tcp> {
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
        })
    }

    /// Send `request`, whose URI is a path relative to the base, such as `rpc`.
    pub(super) async fn send(
        &self,
        request: http::Request<Bytes>,
    ) -> Result<http::Response<Body>, String> {
        let (parts, body) = request.into_parts();
        let url = self
            .base
            .join(&parts.uri.to_string())
            .map_err(|e| format!("{e:#}"))?;
        let response = self
            .http
            .request(parts.method, url)
            .headers(parts.headers)
            .body(body)
            .send()
            .await
            .map_err(|e| format!("{e:#}"))?;
        let response: http::Response<reqwest::Body> = response.into();
        Ok(response.map(|body| body.map_err(std::io::Error::other).boxed()))
    }
}
