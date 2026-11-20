//! The client's end of HTTP over TCP: the server at a base URL, each endpoint a path under it.

use std::net::IpAddr;
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
    pub(super) http: reqwest::Client,
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
        not_in_the_clear(&base, sends_a_token(&auth) || carry_a_token(&headers))?;
        if !base.path().ends_with('/') {
            base.set_path(&format!("{}/", base.path()));
        }
        Ok(Tcp {
            http: reqwest::Client::new(),
            base,
            credentials: Arc::new(Credentials::new(auth)),
            headers: Arc::new(RwLock::new(headers)),
        })
    }

    /// Send `auth` with every request from now on, in place of what was sent until now.
    pub(super) fn set_auth(&self, auth: Auth) -> Result<(), Error> {
        not_in_the_clear(&self.base, sends_a_token(&auth))?;
        self.credentials.set(auth);
        Ok(())
    }

    /// Send `headers` with every request from now on, in place of those sent until now.
    pub(super) fn set_headers(&self, headers: HeaderMap) -> Result<(), Error> {
        not_in_the_clear(&self.base, carry_a_token(&headers))?;
        *self.headers.write().unwrap_or_else(PoisonError::into_inner) = headers;
        Ok(())
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
            .http
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

/// Whether `auth` has a token sent with each request.
fn sends_a_token(auth: &Auth) -> bool {
    !matches!(auth, Auth::None)
}

/// Whether `headers` have a token among them: an `Authorization`, which goes as given while no
/// auth's token takes its place ([`authorized`]).
fn carry_a_token(headers: &HeaderMap) -> bool {
    headers.contains_key(header::AUTHORIZATION)
}

/// Refuses a `token` to `base` where it would go in the clear: over `http` to any host but
/// this machine's loopback, where a local server is (the protocol) and no wire.
fn not_in_the_clear(base: &Url, token: bool) -> Result<(), Error> {
    if !token || base.scheme() == "https" {
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

    fn headers(name: &'static str, value: &'static str) -> HeaderMap {
        HeaderMap::from_iter([(
            header::HeaderName::from_static(name),
            header::HeaderValue::from_static(value),
        )])
    }

    #[test]
    fn a_token_in_a_header_goes_over_http_to_the_loopback_alone() {
        // 192.0.2.1 is an address for documentation (RFC 5737): nothing is sent to it here.
        let elsewhere = "http://192.0.2.1:9/";
        let connect = |url| Tcp::connect(url, Auth::None, headers("authorization", "Bearer t"));
        let refused = connect(elsewhere).err().unwrap();
        assert_eq!(
            refused.to_string(),
            "a token would go in the clear to http://192.0.2.1:9/: reach the server over https"
        );
        assert!(connect("https://192.0.2.1:9/").is_ok());
        assert!(connect("http://127.0.0.1:9/").is_ok());
        assert!(connect("http://[::1]:9/").is_ok());
        assert!(connect("http://localhost:9/").is_ok());

        // A header that is no token goes anywhere.
        let named = Tcp::connect(elsewhere, Auth::None, headers("virtx-project", "p"));
        assert!(named.is_ok());
    }

    #[test]
    fn headers_set_later_are_refused_a_token_as_at_connect_and_left_as_they_were() {
        let url = "http://192.0.2.1:9/";
        let tcp = Tcp::connect(url, Auth::None, headers("virtx-project", "p")).unwrap();
        let refused = tcp
            .set_headers(headers("authorization", "Bearer t"))
            .unwrap_err();
        assert!(refused.to_string().contains("in the clear"), "{refused}");
        assert_eq!(current(&tcp.headers), headers("virtx-project", "p"));

        tcp.set_headers(headers("virtx-project", "q")).unwrap();
        assert_eq!(current(&tcp.headers), headers("virtx-project", "q"));

        let local = Tcp::connect("http://127.0.0.1:9/", Auth::None, HeaderMap::new()).unwrap();
        local
            .set_headers(headers("authorization", "Bearer t"))
            .unwrap();
        assert_eq!(
            current(&local.headers),
            headers("authorization", "Bearer t")
        );
    }
}
