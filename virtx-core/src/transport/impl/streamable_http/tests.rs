//! Both ends of Streamable HTTP, and the server against `PROTOCOL.md` by hand.

use std::sync::Arc;

use axum::{extract::State, middleware::Next, response::IntoResponse};
use ciborium::{Value, cbor};
use futures_util::StreamExt;
use http::{HeaderMap, StatusCode};
use tokio::{net::TcpListener, task::JoinSet};

use super::*;
use crate::method::{Progress, Request};
use crate::transport::{
    Dispatcher, Event, TransportClient, r#impl::frame::Parser, testing::methods,
};

/// The user a [`Tokens`] request was let in as.
#[derive(Clone)]
struct User(String);

/// `user`: answers the [`User`] of the request.
struct WhoAmI;

impl Request for WhoAmI {
    const NAME: &'static str = "user";
    type Args = [(); 0];
    type Output = String;
}

/// Lets in a request whose bearer token names one of `users`, as that [`User`]. Middleware in
/// front of a [`Server`], as a mounting server's would be.
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

/// A client of the server at `url`.
fn connect(url: &str) -> TransportClient {
    let (outgoing, incoming) = super::connect(url).unwrap();
    TransportClient::new(outgoing, incoming)
}

const BOTH: &str = "application/cbor, application/cbor-seq";

#[tokio::test]
async fn a_client_calls() {
    let server = Arc::new(Server::new(Dispatcher::new(methods())));
    let router = axum::Router::new().route(
        "/rpc",
        axum::routing::any(move |request: axum::extract::Request| {
            let server = server.clone();
            async move { server.handle(request).await }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/rpc", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await });
    let client = connect(&url);
    let answer: String = client.request("echo", ["hi"]).await.unwrap();
    assert_eq!(answer, "hi");
    client.close().await;
}

#[tokio::test]
async fn concurrent_calls_each_get_their_own_response() {
    let server = Arc::new(Server::new(Dispatcher::new(methods())));
    let router = axum::Router::new().route(
        "/rpc",
        axum::routing::any(move |request: axum::extract::Request| {
            let server = server.clone();
            async move { server.handle(request).await }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/rpc", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await });
    let client = Arc::new(connect(&url));
    let mut calls = JoinSet::new();
    for i in 0..20 {
        let client = Arc::clone(&client);
        calls.spawn(async move {
            let answer: String = client.request("echo", [i.to_string()]).await.unwrap();
            (i, answer)
        });
    }
    for (i, answer) in calls.join_all().await {
        assert_eq!(answer, i.to_string());
    }
    Arc::into_inner(client).unwrap().close().await;
}

#[tokio::test]
async fn a_client_receives_progress_over_the_stream() {
    let server = Arc::new(Server::new(Dispatcher::new(methods())));
    let router = axum::Router::new().route(
        "/rpc",
        axum::routing::any(move |request: axum::extract::Request| {
            let server = server.clone();
            async move { server.handle(request).await }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/rpc", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await });
    let client = connect(&url);
    let events: Vec<_> = client
        .request_with_progress::<u32, String>("count", [3])
        .map(Result::unwrap)
        .collect()
        .await;
    assert_eq!(
        events,
        [
            Event::Progress(count(1, 3)),
            Event::Progress(count(2, 3)),
            Event::Progress(count(3, 3)),
            Event::Output(3)
        ]
    );
    client.close().await;
}

#[tokio::test]
async fn a_failed_post_fails_its_call_and_not_the_client() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/rpc", listener.local_addr().unwrap());
    drop(listener);
    let client = connect(&url);
    for _ in 0..2 {
        let error = client.request::<String>("echo", ["hi"]).await.unwrap_err();
        assert!(client.is_connected(), "{error}");
    }
    client.close().await;
}

#[test]
fn connect_refuses_anything_but_an_http_url() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let _guard = runtime.enter();
    for url in ["not a url", "file:///tmp/x"] {
        assert!(super::connect(url).is_err(), "{url}");
    }
}

#[tokio::test]
async fn a_request_without_notifications_answers_one_message() {
    let server = Arc::new(Server::new(Dispatcher::new(methods())));
    let router = axum::Router::new().route(
        "/rpc",
        axum::routing::any(move |request: axum::extract::Request| {
            let server = server.clone();
            async move { server.handle(request).await }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/rpc", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await });
    let mut echo = Vec::new();
    ciborium::into_writer(
        &cbor!({"jsonrpc" => "2.0", "id" => 1, "method" => "echo", "params" => ["hi"]}).unwrap(),
        &mut echo,
    )
    .unwrap();
    let response = reqwest::Client::new()
        .post(&url)
        .header("content-type", CBOR)
        .header("accept", BOTH)
        .body(echo)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["content-type"], CBOR);
    let answer: Value = ciborium::from_reader(&response.bytes().await.unwrap()[..]).unwrap();
    assert_eq!(
        answer,
        cbor!({"jsonrpc" => "2.0", "id" => 1, "result" => "hi"}).unwrap()
    );
}

#[tokio::test]
async fn a_request_with_notifications_answers_a_stream_ending_in_its_response() {
    let server = Arc::new(Server::new(Dispatcher::new(methods())));
    let router = axum::Router::new().route(
        "/rpc",
        axum::routing::any(move |request: axum::extract::Request| {
            let server = server.clone();
            async move { server.handle(request).await }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/rpc", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await });
    let mut count = Vec::new();
    ciborium::into_writer(
        &cbor!({"jsonrpc" => "2.0", "id" => 1, "method" => "count", "params" => [2]}).unwrap(),
        &mut count,
    )
    .unwrap();
    let response = reqwest::Client::new()
        .post(&url)
        .header("content-type", CBOR)
        .header("accept", BOTH)
        .body(count)
        .send()
        .await
        .unwrap();
    assert_eq!(response.headers()["content-type"], CBOR_SEQ);
    let mut parser = Parser::default();
    parser.push(&response.bytes().await.unwrap());
    let messages: Vec<Value> = std::iter::from_fn(|| parser.next_message())
        .map(|message| ciborium::from_reader(&message[..]).unwrap())
        .collect();
    let progress = |progress| {
        cbor!({
            "jsonrpc" => "2.0",
            "method" => "notifications/progress",
            "params" => {"requestId" => 1, "progress" => progress, "total" => 2},
        })
        .unwrap()
    };
    assert_eq!(
        messages,
        [
            progress(1),
            progress(2),
            cbor!({"jsonrpc" => "2.0", "id" => 1, "result" => 2}).unwrap(),
        ]
    );
}

#[tokio::test]
async fn a_client_that_takes_no_stream_gets_one_message() {
    let server = Arc::new(Server::new(Dispatcher::new(methods())));
    let router = axum::Router::new().route(
        "/rpc",
        axum::routing::any(move |request: axum::extract::Request| {
            let server = server.clone();
            async move { server.handle(request).await }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/rpc", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await });
    let mut count = Vec::new();
    ciborium::into_writer(
        &cbor!({"jsonrpc" => "2.0", "id" => 1, "method" => "count", "params" => [2]}).unwrap(),
        &mut count,
    )
    .unwrap();
    let response = reqwest::Client::new()
        .post(&url)
        .header("content-type", CBOR)
        .header("accept", CBOR)
        .body(count)
        .send()
        .await
        .unwrap();
    assert_eq!(response.headers()["content-type"], CBOR);
    let answer: Value = ciborium::from_reader(&response.bytes().await.unwrap()[..]).unwrap();
    assert_eq!(
        answer,
        cbor!({"jsonrpc" => "2.0", "id" => 1, "result" => 2}).unwrap()
    );
}

#[tokio::test]
async fn a_notification_is_accepted() {
    let server = Arc::new(Server::new(Dispatcher::new(methods())));
    let router = axum::Router::new().route(
        "/rpc",
        axum::routing::any(move |request: axum::extract::Request| {
            let server = server.clone();
            async move { server.handle(request).await }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/rpc", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await });
    let mut notification = Vec::new();
    ciborium::into_writer(
        &cbor!({"jsonrpc" => "2.0", "method" => "echo", "params" => ["hi"]}).unwrap(),
        &mut notification,
    )
    .unwrap();
    let response = reqwest::Client::new()
        .post(&url)
        .header("content-type", CBOR)
        .header("accept", BOTH)
        .body(notification)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 202);
}

#[tokio::test]
async fn only_a_post_is_allowed() {
    let server = Arc::new(Server::new(Dispatcher::new(methods())));
    let router = axum::Router::new().route(
        "/rpc",
        axum::routing::any(move |request: axum::extract::Request| {
            let server = server.clone();
            async move { server.handle(request).await }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/rpc", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await });
    let http = reqwest::Client::new();
    for response in [
        http.get(&url).send().await.unwrap(),
        http.delete(&url).send().await.unwrap(),
    ] {
        assert_eq!(response.status(), 405);
        assert_eq!(response.headers()["allow"], "POST");
    }
}

#[tokio::test]
async fn middleware_refuses_a_request_before_it_reaches_a_method() {
    let server = Arc::new(Server::new(Dispatcher::new(methods())));
    let tokens = Tokens {
        users: vec!["alice"],
    };
    let router = axum::Router::new()
        .route(
            "/rpc",
            axum::routing::any(move |request: axum::extract::Request| {
                let server = server.clone();
                async move { server.handle(request).await }
            }),
        )
        .layer(axum::middleware::from_fn_with_state(tokens, self::tokens));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/rpc", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await });
    let mut echo = Vec::new();
    ciborium::into_writer(
        &cbor!({"jsonrpc" => "2.0", "id" => 1, "method" => "echo", "params" => ["hi"]}).unwrap(),
        &mut echo,
    )
    .unwrap();
    let http = reqwest::Client::new();
    let anonymous = http
        .post(&url)
        .header("content-type", CBOR)
        .header("accept", CBOR)
        .body(echo.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(anonymous.status(), 401);
    let stranger = http
        .post(&url)
        .header("content-type", CBOR)
        .header("accept", CBOR)
        .header("authorization", "Bearer mallory")
        .body(echo)
        .send()
        .await
        .unwrap();
    assert_eq!(stranger.status(), 403);
}

#[tokio::test]
async fn what_middleware_puts_in_a_request_reaches_its_method() {
    let mut methods = methods();
    methods.add::<WhoAmI, _, _>(|_, extensions| async move {
        Ok(extensions.get::<User>().expect("a user").0.clone())
    });
    let server = Arc::new(Server::new(Dispatcher::new(methods)));
    let tokens = Tokens {
        users: vec!["alice", "bob"],
    };
    let router = axum::Router::new()
        .route(
            "/rpc",
            axum::routing::any(move |request: axum::extract::Request| {
                let server = server.clone();
                async move { server.handle(request).await }
            }),
        )
        .layer(axum::middleware::from_fn_with_state(tokens, self::tokens));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/rpc", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await });
    let mut user = Vec::new();
    ciborium::into_writer(
        &cbor!({"jsonrpc" => "2.0", "id" => 1, "method" => "user", "params" => []}).unwrap(),
        &mut user,
    )
    .unwrap();
    let http = reqwest::Client::new();
    for name in ["alice", "bob"] {
        let response = http
            .post(&url)
            .header("content-type", CBOR)
            .header("accept", CBOR)
            .header("authorization", format!("Bearer {name}"))
            .body(user.clone())
            .send()
            .await
            .unwrap();
        let answer: Value = ciborium::from_reader(&response.bytes().await.unwrap()[..]).unwrap();
        assert_eq!(
            answer,
            cbor!({"jsonrpc" => "2.0", "id" => 1, "result" => name}).unwrap()
        );
    }
}

/// What `count` sends as its `n`th progress, counting to `to`.
fn count(n: u32, to: u32) -> Progress<String> {
    Progress {
        progress: n.into(),
        total: Some(to.into()),
        content: None,
    }
}
