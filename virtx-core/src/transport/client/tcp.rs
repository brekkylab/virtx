//! The client's end of HTTP over TCP: the server at a base URL, each endpoint a path under it.

use std::net::IpAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, PoisonError, RwLock};

use http::{HeaderMap, header};
use http_body_util::BodyExt;
use reqwest::{RequestBuilder, Url};

use super::{Body, credentials::Credentials};
use crate::{Auth, Error};

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
    /// What requests go out through, in turn ([`Tcp::client`]): one, until
    /// [`set_connections`](Tcp::set_connections) says otherwise.
    clients: RwLock<Vec<reqwest::Client>>,
    next: AtomicUsize,
    /// What a port's WebSocket is opened with: HTTP/1.1 alone, the version it upgrades from.
    pub(super) upgrades: reqwest::Client,
    /// Ends in `/`, so a path joins under it.
    pub(super) base: Url,
    pub(super) credentials: Arc<Credentials>,
    pub(super) headers: Shared<HeaderMap>,
}

impl Tcp {
    /// The server whose base URL is `url`, an `http` or `https` URL, with `auth` and `headers`
    /// sent on every request to it. Nothing is sent until the first request.
    pub(super) fn connect(url: &str, auth: Auth, headers: HeaderMap) -> Result<Tcp, Error> {
        let mut base = Url::parse(url).map_err(|e| Error::Client(format!("{url}: {e}")))?;
        if !matches!(base.scheme(), "http" | "https") {
            return Err(Error::Client(format!("not an http or https URL: {base}")));
        }
        not_in_the_clear(&base, &auth)?;
        if !base.path().ends_with('/') {
            base.set_path(&format!("{}/", base.path()));
        }
        let upgrades = reqwest::Client::builder()
            .http1_only()
            .build()
            .map_err(|e| Error::Client(format!("an HTTP/1.1 client: {e}")))?;
        Ok(Tcp {
            clients: RwLock::new(vec![reqwest::Client::new()]),
            next: AtomicUsize::new(0),
            upgrades,
            base,
            credentials: Arc::new(Credentials::new(auth)),
            headers: Arc::new(RwLock::new(headers)),
        })
    }

    /// Send `auth` with every request from now on, in place of what was sent until now.
    pub(super) fn set_auth(&self, auth: Auth) -> Result<(), Error> {
        not_in_the_clear(&self.base, &auth)?;
        self.credentials.set(auth);
        Ok(())
    }

    /// Send `headers` with every request from now on, in place of those sent until now.
    pub(super) fn set_headers(&self, headers: HeaderMap) {
        *self.headers.write().unwrap_or_else(PoisonError::into_inner) = headers;
    }

    /// Send the requests from now on through `connections` HTTP clients in turn, each with a
    /// pool of its own. Over HTTPS each speaks HTTP/2 where the server does, a request one stream
    /// of its one connection: a burst of requests from a new client pays that many TLS handshakes
    /// rather than one each, nor is all of it held up by one connection that stalls. Refused for
    /// none.
    pub(super) fn set_connections(&self, connections: usize) -> Result<(), Error> {
        if connections == 0 {
            return Err(Error::Client("connections: 0 sends nothing".into()));
        }
        *self.clients.write().unwrap_or_else(PoisonError::into_inner) =
            (0..connections).map(|_| reqwest::Client::new()).collect();
        Ok(())
    }

    /// The client the next request goes out through.
    fn client(&self) -> reqwest::Client {
        let clients = self.clients.read().unwrap_or_else(PoisonError::into_inner);
        clients[self.next.fetch_add(1, Ordering::Relaxed) % clients.len()].clone()
    }

    /// Send `request`, whose URI is a path under the base.
    pub(super) async fn send(
        &self,
        request: http::Request<Body>,
    ) -> Result<http::Response<Body>, Error> {
        let (parts, body) = request.into_parts();
        let url = self
            .base
            .join(parts.uri.to_string().trim_start_matches('/'))
            .map_err(|e| Error::Client(format!("{}: {e}", parts.uri)))?;
        // The request's own headers win over the ones sent with every request.
        let mut headers = current(&self.headers);
        headers.extend(parts.headers);
        let request = self
            .client()
            .request(parts.method, url)
            .body(reqwest::Body::wrap(body));
        let token = self.credentials.token().await.map_err(Error::Client)?;
        let response = authorized(request, headers, token.as_deref())
            .send()
            .await
            .map_err(|e| Error::Transport(format!("{e:#}")))?;
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            self.credentials.refused();
        }
        let response: http::Response<reqwest::Body> = response.into();
        Ok(response.map(|body| body.map_err(std::io::Error::other).boxed()))
    }
}

/// `request` with `headers`, and `token` as its `Authorization` in place of any among them.
pub(super) fn authorized(
    request: RequestBuilder,
    mut headers: HeaderMap,
    token: Option<&str>,
) -> RequestBuilder {
    match token {
        None => request.headers(headers),
        Some(token) => {
            headers.remove(header::AUTHORIZATION);
            request.headers(headers).bearer_auth(token)
        }
    }
}

/// Refuses `auth` to `base` when its token would go in the clear: over `http` to any host but this
/// machine's loopback, where a local server is (the protocol) and no wire.
fn not_in_the_clear(base: &Url, auth: &Auth) -> Result<(), Error> {
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
    if !loopback {
        return Err(Error::Client(format!(
            "a token would go in the clear to {base}: reach the server over https"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sent(headers: &[(&str, &str)], token: Option<&str>) -> HeaderMap {
        let mut given = HeaderMap::new();
        for (name, value) in headers {
            let name = header::HeaderName::from_bytes(name.as_bytes()).unwrap();
            given.append(name, value.parse().unwrap());
        }
        let request = reqwest::Client::new().get("http://127.0.0.1:9/");
        authorized(request, given, token)
            .build()
            .unwrap()
            .headers()
            .clone()
    }

    #[test]
    fn a_token_takes_the_place_of_an_authorization_header_and_leaves_the_rest() {
        let given = [("authorization", "Bearer mine"), ("virtx-project", "p")];
        let headers = sent(&given, Some("t"));
        let authorization: Vec<_> = headers.get_all(header::AUTHORIZATION).iter().collect();
        assert_eq!(authorization, ["Bearer t"]);
        assert_eq!(headers["virtx-project"], "p");
    }

    #[test]
    fn without_a_token_the_headers_go_as_given() {
        let headers = sent(&[("authorization", "Bearer mine")], None);
        assert_eq!(headers[header::AUTHORIZATION], "Bearer mine");
    }
}
