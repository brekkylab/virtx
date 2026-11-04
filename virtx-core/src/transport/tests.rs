//! Both ends together: the [`testing`](crate::testing) server over HTTP, served by axum, and
//! over an in-memory HTTP/2 connection as stdio serves one; and what every endpoint shares on
//! either, such as the credentials sent with each request.

use ::http::{HeaderMap, StatusCode};
use axum::{Extension, Router, extract::State, middleware::Next, response::IntoResponse};
use hyper::service::Service as _;
use hyper_util::service::TowerToHyperService;
use tokio::net::TcpListener;

use super::{TransportClient, http, serve_stdio};
use crate::{Auth, Error, volume::VolumeListOutputItem};

/// `router` served over an in-memory HTTP/2 connection, as stdio serves, and a client of it.
pub(crate) fn piped(router: Router) -> TransportClient {
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (input, output) = tokio::io::split(server);
    let service = TowerToHyperService::new(router);
    let handle = move |request| {
        let service = service.clone();
        async move { service.call(request).await.unwrap_or_else(|e| match e {}) }
    };
    tokio::spawn(serve_stdio(handle, input, output));
    TransportClient::over(client)
}

/// `router` served over HTTP on this machine's loopback, and its base URL.
pub(crate) async fn serve(router: Router) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await });
    url
}

/// A client of a test server of its own over each transport.
pub(crate) async fn clients() -> [TransportClient; 2] {
    let url = serve(crate::testing::router()).await;
    [
        TransportClient::connect(&url, Auth::None, HeaderMap::new()).unwrap(),
        piped(crate::testing::router()),
    ]
}

/// The user a [`tokens`] request was let in as.
#[derive(Clone)]
struct User(String);

/// Lets in a request whose bearer token names one of `users`, as that [`User`]: middleware in
/// front of the endpoints, as a server's would be.
#[derive(Clone)]
struct Tokens {
    users: Vec<&'static str>,
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get("authorization")?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}

async fn tokens(
    State(tokens): State<Tokens>,
    mut request: axum::extract::Request,
    next: Next,
) -> axum::response::Response {
    let Some(user) = bearer(request.headers()).map(str::to_owned) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    if !tokens.users.contains(&user.as_str()) {
        return StatusCode::FORBIDDEN.into_response();
    }
    request.extensions_mut().insert(User(user));
    next.run(request).await
}

/// `/user`, answering whom the request's token names, behind `users`' tokens, and its base URL.
async fn serve_users(users: Vec<&'static str>) -> String {
    let router = Router::new()
        .route(
            "/user",
            axum::routing::get(|Extension(User(user)): Extension<User>| async move {
                serde_json::to_vec(&user).unwrap()
            }),
        )
        .layer(axum::middleware::from_fn_with_state(
            Tokens { users },
            tokens,
        ));
    serve(router).await
}

fn token(token: &str) -> Auth {
    Auth::Bearer {
        token: token.into(),
    }
}

/// `Authorization: Bearer {token}`, as a header of the caller's own.
fn authorization(token: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert("authorization", format!("Bearer {token}").parse().unwrap());
    headers
}

async fn user(client: &TransportClient) -> Result<String, Error> {
    http::get(client, "/user").await
}

#[tokio::test]
async fn the_auth_given_at_connect_goes_with_every_request() {
    let url = serve_users(vec!["alice"]).await;
    let client = TransportClient::connect(&url, token("alice"), HeaderMap::new()).unwrap();
    for _ in 0..2 {
        assert_eq!(user(&client).await.unwrap(), "alice");
    }
}

#[tokio::test]
async fn the_auth_set_later_goes_with_the_requests_after() {
    let url = serve_users(vec!["alice", "bob"]).await;
    let client = TransportClient::connect(&url, token("alice"), HeaderMap::new()).unwrap();
    assert_eq!(user(&client).await.unwrap(), "alice");

    client.set_auth(token("bob")).unwrap();
    assert_eq!(user(&client).await.unwrap(), "bob");

    // The old one is gone with the new, not kept under it.
    client.set_auth(Auth::None).unwrap();
    let error = user(&client).await.unwrap_err();
    assert!(matches!(error, Error::Status(401, _)), "{error:?}");
    client.set_auth(token("carol")).unwrap();
    let error = user(&client).await.unwrap_err();
    assert!(matches!(error, Error::Status(403, _)), "{error:?}");
}

#[tokio::test]
async fn the_headers_given_at_connect_and_set_later_go_with_the_requests_after() {
    // The token in a header of the caller's own, as to a server that reads it from one.
    let url = serve_users(vec!["alice", "bob"]).await;
    let client = TransportClient::connect(&url, Auth::None, authorization("alice")).unwrap();
    assert_eq!(user(&client).await.unwrap(), "alice");

    client.set_headers(authorization("bob"));
    assert_eq!(user(&client).await.unwrap(), "bob");
    client.set_headers(HeaderMap::new());
    let error = user(&client).await.unwrap_err();
    assert!(matches!(error, Error::Status(401, _)), "{error:?}");
}

#[tokio::test]
async fn the_auth_goes_over_an_authorization_header() {
    let url = serve_users(vec!["alice", "bob"]).await;
    let client = TransportClient::connect(&url, token("alice"), authorization("bob")).unwrap();
    assert_eq!(user(&client).await.unwrap(), "alice");
}

#[test]
fn a_token_does_not_go_in_the_clear_to_another_machine() {
    let with_token = |url| TransportClient::connect(url, token("t"), HeaderMap::new());
    assert!(with_token("http://example.com/").is_err());
    for url in [
        "https://example.com/",
        "http://127.0.0.1:9/",
        "http://localhost:9/",
        "http://[::1]:9/",
    ] {
        assert!(with_token(url).is_ok(), "{url}");
    }
    let client = TransportClient::connect("http://example.com/", Auth::None, HeaderMap::new());
    assert!(client.unwrap().set_auth(token("t")).is_err());
}

#[test]
fn connect_refuses_anything_but_an_http_url() {
    for url in ["not a url", "file:///tmp/x"] {
        assert!(
            TransportClient::connect(url, Auth::None, HeaderMap::new()).is_err(),
            "{url}"
        );
    }
}

#[tokio::test]
async fn the_endpoints_are_under_the_base_url_with_or_without_its_slash() {
    let url = serve(Router::new().nest("/base", crate::testing::router())).await;
    for base in [format!("{url}base"), format!("{url}base/")] {
        let client = TransportClient::connect(&base, Auth::None, HeaderMap::new()).unwrap();
        let volumes: Vec<VolumeListOutputItem> = http::get(&client, "/volumes/").await.unwrap();
        assert!(volumes.iter().any(|volume| volume.name == "s"));
    }
}

#[tokio::test]
async fn a_request_to_no_server_fails() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    drop(listener);
    let client = TransportClient::connect(&url, Auth::None, HeaderMap::new()).unwrap();
    for _ in 0..2 {
        let error = http::get::<Vec<VolumeListOutputItem>>(&client, "/volumes/")
            .await
            .unwrap_err();
        assert!(matches!(error, Error::Transport(_)), "{error:?}");
    }
}

#[tokio::test]
async fn a_refusal_reads_its_code_and_message() {
    for client in clients().await {
        let error = http::get::<VolumeListOutputItem>(&client, "/volumes/nothing")
            .await
            .unwrap_err();
        let Error::Status(404, message) = &error else {
            panic!("not a 404: {error:?}");
        };
        assert!(message.starts_with("not_found: "), "{message}");
        client.close().await;
    }
}

#[cfg(unix)]
#[tokio::test]
async fn a_server_that_exits_fails_every_request() {
    let client = TransportClient::spawn(tokio::process::Command::new("true")).unwrap();
    for _ in 0..2 {
        assert!(
            http::get::<Vec<VolumeListOutputItem>>(&client, "/volumes/")
                .await
                .is_err()
        );
    }
    client.close().await;
}

#[tokio::test]
async fn serve_returns_once_its_client_is_gone() {
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (input, output) = tokio::io::split(server);
    let service = TowerToHyperService::new(crate::testing::router());
    let handle = move |request| {
        let service = service.clone();
        async move { service.call(request).await.unwrap_or_else(|e| match e {}) }
    };
    let served = tokio::spawn(serve_stdio(handle, input, output));
    let client = TransportClient::over(client);
    let _: Vec<VolumeListOutputItem> = http::get(&client, "/volumes/").await.unwrap();
    client.close().await;
    served.await.unwrap().unwrap();
}
