//! The client's end of HTTP over TCP: the server at a base URL, each endpoint a path under it.

use std::net::IpAddr;
use std::sync::{Arc, PoisonError, RwLock};

use http::{HeaderMap, header};
use http_body_util::BodyExt;
use reqwest::{RequestBuilder, Url};

use super::Body;
use crate::Auth;

/// What is sent with every request, read as each is sent, so that a change reaches the next
/// one: a forwarder opened before keeps a hold on it too.
pub(super) type Shared<T> = Arc<RwLock<T>>;

/// What `shared` holds, as sent now.
pub(super) fn current<T: Clone>(shared: &Shared<T>) -> T {
    shared
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}

/// The server at a base URL. Nothing to close: the server keeps nothing between requests.
pub(super) struct Tcp {
    pub(super) http: reqwest::Client,
    /// Ends in `/`, so a path joins under it.
    pub(super) base: Url,
    pub(super) auth: Shared<Auth>,
    pub(super) headers: Shared<HeaderMap>,
}

impl Tcp {
    /// The server whose base URL is `url`, an `http` or `https` URL, with `auth` and `headers`
    /// sent on every request to it. Nothing is sent until the first request.
    pub(super) fn connect(url: &str, auth: Auth, headers: HeaderMap) -> anyhow::Result<Tcp> {
        let mut base = Url::parse(url)?;
        anyhow::ensure!(
            matches!(base.scheme(), "http" | "https"),
            "not an http or https URL: {base}"
        );
        not_in_the_clear(&base, &auth)?;
        if !base.path().ends_with('/') {
            base.set_path(&format!("{}/", base.path()));
        }
        Ok(Tcp {
            http: reqwest::Client::new(),
            base,
            auth: Arc::new(RwLock::new(auth)),
            headers: Arc::new(RwLock::new(headers)),
        })
    }

    /// Send `auth` with every request from now on, in place of what was sent until now.
    pub(super) fn set_auth(&self, auth: Auth) -> anyhow::Result<()> {
        not_in_the_clear(&self.base, &auth)?;
        *self.auth.write().unwrap_or_else(PoisonError::into_inner) = auth;
        Ok(())
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
        let request = self
            .http
            .request(parts.method, url)
            .body(reqwest::Body::wrap(body));
        let response = authorized(request, headers, &current(&self.auth))
            .send()
            .await
            .map_err(|e| format!("{e:#}"))?;
        let response: http::Response<reqwest::Body> = response.into();
        Ok(response.map(|body| body.map_err(std::io::Error::other).boxed()))
    }
}

/// `request` with `headers`, and `auth`'s `Authorization` in place of any among them.
pub(super) fn authorized(
    request: RequestBuilder,
    mut headers: HeaderMap,
    auth: &Auth,
) -> RequestBuilder {
    match auth {
        Auth::None => request.headers(headers),
        Auth::Bearer { token } => {
            headers.remove(header::AUTHORIZATION);
            request.headers(headers).bearer_auth(token)
        }
    }
}

/// Refuses `auth` to `base` when its token would go in the clear: over `http` to any host but
/// this machine's loopback, where a local server is (`PROTOCOL.md`) and no wire.
fn not_in_the_clear(base: &Url, auth: &Auth) -> anyhow::Result<()> {
    if matches!(auth, Auth::None) || base.scheme() == "https" {
        return Ok(());
    }
    let loopback = base.host_str().is_some_and(|host| {
        host == "localhost"
            || host
                .trim_matches(['[', ']'])
                .parse::<IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    });
    anyhow::ensure!(
        loopback,
        "a token would go in the clear to {base}: reach the server over https"
    );
    Ok(())
}
