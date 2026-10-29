//! The client's end of HTTP over TCP: the server at a base URL, each endpoint a path under it.

use http_body_util::BodyExt;

use super::Body;

/// The header a request names its organization in, on a server that keeps organizations
/// apart (PROTOCOL.md, Authentication).
const ORG_HEADER: &str = "virtx-org";

/// The server at a base URL. Nothing to close: the server keeps nothing between requests.
pub(super) struct Tcp {
    pub(super) http: reqwest::Client,
    /// Ends in `/`, so a path joins under it.
    pub(super) base: reqwest::Url,
}

impl Tcp {
    /// The server whose base URL is `url`, an `http` or `https` URL. Every request carries
    /// `token` as `Authorization: Bearer`, and `org` as `virtx-org`, when given.
    /// Nothing is sent until the first request.
    pub(super) fn connect(
        url: &str,
        token: Option<&str>,
        org: Option<&str>,
    ) -> anyhow::Result<Tcp> {
        let mut base = reqwest::Url::parse(url)?;
        anyhow::ensure!(
            matches!(base.scheme(), "http" | "https"),
            "not an http or https URL: {base}"
        );
        if !base.path().ends_with('/') {
            base.set_path(&format!("{}/", base.path()));
        }
        let mut headers = http::HeaderMap::new();
        if let Some(token) = token {
            let mut value = http::HeaderValue::from_str(&format!("Bearer {token}"))
                .map_err(|_| anyhow::anyhow!("the token is not one an HTTP header can carry"))?;
            value.set_sensitive(true);
            headers.insert(http::header::AUTHORIZATION, value);
        }
        if let Some(org) = org {
            let value = http::HeaderValue::from_str(org)
                .map_err(|_| anyhow::anyhow!("the org is not one an HTTP header can carry"))?;
            headers.insert(ORG_HEADER, value);
        }
        Ok(Tcp {
            http: reqwest::Client::builder()
                .default_headers(headers)
                .build()?,
            base,
        })
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
        let response = self
            .http
            .request(parts.method, url)
            .headers(parts.headers)
            .body(reqwest::Body::wrap(body))
            .send()
            .await
            .map_err(|e| format!("{e:#}"))?;
        let response: http::Response<reqwest::Body> = response.into();
        Ok(response.map(|body| body.map_err(std::io::Error::other).boxed()))
    }
}
