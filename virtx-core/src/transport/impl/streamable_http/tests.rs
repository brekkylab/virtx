//! Both ends of Streamable HTTP, and the server against `PROTOCOL.md` by hand.

use jsonrpsee::{RpcModule, rpc_params};
use tokio::net::TcpListener;

use super::*;
use crate::transport::{Dispatcher, Notifier, Transport, TransportClient};

/// A [`Server`] on localhost, serving `echo`, `count` (which notifies `counted` before it
/// answers) and `session` (which answers the request's session id).
async fn serve() -> String {
    let mut module = RpcModule::new(());
    module
        .register_method("echo", |params, _, _| params.one::<String>())
        .unwrap();
    module
        .register_async_method("count", |params, _, extensions| async move {
            let to: u32 = params.one()?;
            let notifier = extensions.get::<Notifier>().expect("a notifier");
            for i in 1..=to {
                notifier.notify("counted", i);
                tokio::task::yield_now().await;
            }
            jsonrpsee::core::RpcResult::Ok(to)
        })
        .unwrap();
    module
        .register_method("session", |_, _, extensions| {
            extensions.get::<SessionId>().expect("a session").0.clone()
        })
        .unwrap();
    let server = Server::with_origins(
        Dispatcher::new(module),
        vec!["https://allowed.example".into()],
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/rpc", listener.local_addr().unwrap());
    let router = axum::Router::new().route(
        "/rpc",
        axum::routing::any(move |request: axum::extract::Request| {
            let server = server.clone();
            async move { server.handle(request).await }
        }),
    );
    tokio::spawn(async move { axum::serve(listener, router).await });
    url
}

/// A `POST` of `body` to `url`, accepting what `accept` says, in `session` if one is given.
async fn post(url: &str, body: &str, accept: &str, session: Option<&str>) -> reqwest::Response {
    let mut request = reqwest::Client::new()
        .post(url)
        .header("content-type", "application/json")
        .header("accept", accept)
        .body(body.to_owned());
    if let Some(id) = session {
        request = request.header(SESSION_HEADER, id);
    }
    request.send().await.unwrap()
}

/// A client of the server at `url`.
fn connect(url: &str) -> TransportClient {
    let (sender, receiver, transport) = Transport::connect(url).unwrap();
    TransportClient::new(sender, receiver, transport)
}

const BOTH: &str = "application/json, text/event-stream";

#[tokio::test]
async fn a_client_calls_and_its_session_carries_over() {
    let url = serve().await;
    let session = connect(&url);
    let answer: String = session.request("echo", rpc_params!["hi"]).await.unwrap();
    assert_eq!(answer, "hi");
    let first: String = session.request("session", rpc_params![]).await.unwrap();
    let second: String = session.request("session", rpc_params![]).await.unwrap();
    assert_eq!(first, second);
    session.close().await;
}

#[tokio::test]
async fn a_client_receives_notifications_over_the_stream() {
    let url = serve().await;
    let session = connect(&url);
    let mut counted = session.notifications::<u32>("counted").await.unwrap();
    let answer: u32 = session.request("count", rpc_params![3]).await.unwrap();
    assert_eq!(answer, 3);
    for i in 1..=3 {
        assert_eq!(counted.next().await.unwrap().unwrap(), i);
    }
    session.close().await;
}

#[tokio::test]
async fn a_failed_post_fails_its_call_and_not_the_client() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/rpc", listener.local_addr().unwrap());
    drop(listener);
    let session = connect(&url);
    for _ in 0..2 {
        let error = session
            .request::<String>("echo", rpc_params!["hi"])
            .await
            .unwrap_err();
        assert!(session.is_connected(), "{error}");
    }
    session.close().await;
}

#[test]
fn connect_refuses_anything_but_an_http_url() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let _guard = runtime.enter();
    for url in ["not a url", "file:///tmp/x"] {
        assert!(Transport::connect(url).is_err(), "{url}");
    }
}

#[tokio::test]
async fn a_first_post_opens_a_session_and_answers_json() {
    let url = serve().await;
    let request = r#"{"jsonrpc":"2.0","id":1,"method":"echo","params":["hi"]}"#;
    let response = post(&url, request, BOTH, None).await;
    assert_eq!(response.status(), 200);
    assert!(response.headers().contains_key(SESSION_HEADER));
    assert_eq!(response.headers()["content-type"], "application/json");
    assert_eq!(
        response.text().await.unwrap(),
        r#"{"jsonrpc":"2.0","id":1,"result":"hi"}"#
    );
}

#[tokio::test]
async fn a_request_with_notifications_answers_a_stream_ending_in_its_response() {
    let url = serve().await;
    let request = r#"{"jsonrpc":"2.0","id":1,"method":"count","params":[2]}"#;
    let response = post(&url, request, BOTH, None).await;
    assert_eq!(response.headers()["content-type"], EVENT_STREAM);
    let mut parser = sse::Parser::default();
    let events = parser.feed(&response.bytes().await.unwrap());
    assert_eq!(
        events,
        vec![
            r#"{"jsonrpc":"2.0","method":"counted","params":1}"#,
            r#"{"jsonrpc":"2.0","method":"counted","params":2}"#,
            r#"{"jsonrpc":"2.0","id":1,"result":2}"#,
        ]
    );
}

#[tokio::test]
async fn a_client_that_takes_no_stream_gets_json() {
    let url = serve().await;
    let request = r#"{"jsonrpc":"2.0","id":1,"method":"count","params":[2]}"#;
    let response = post(&url, request, "application/json", None).await;
    assert_eq!(response.headers()["content-type"], "application/json");
    assert_eq!(
        response.text().await.unwrap(),
        r#"{"jsonrpc":"2.0","id":1,"result":2}"#
    );
}

#[tokio::test]
async fn a_notification_is_accepted() {
    let url = serve().await;
    let notification = r#"{"jsonrpc":"2.0","method":"echo","params":["hi"]}"#;
    assert_eq!(post(&url, notification, BOTH, None).await.status(), 202);
}

#[tokio::test]
async fn a_session_ends_with_a_delete_and_is_unknown_after() {
    let url = serve().await;
    let request = r#"{"jsonrpc":"2.0","id":1,"method":"echo","params":["hi"]}"#;
    let opened = post(&url, request, BOTH, None).await;
    let id = opened.headers()[SESSION_HEADER]
        .to_str()
        .unwrap()
        .to_owned();
    assert_eq!(post(&url, request, BOTH, Some(&id)).await.status(), 200);

    let http = reqwest::Client::new();
    let delete = || http.delete(&url).header(SESSION_HEADER, &id).send();
    assert_eq!(delete().await.unwrap().status(), 204);
    assert_eq!(delete().await.unwrap().status(), 404);
    assert_eq!(post(&url, request, BOTH, Some(&id)).await.status(), 404);
}

#[tokio::test]
async fn an_unknown_session_is_not_found() {
    let url = serve().await;
    let request = r#"{"jsonrpc":"2.0","id":1,"method":"echo","params":["hi"]}"#;
    assert_eq!(post(&url, request, BOTH, Some("nope")).await.status(), 404);
}

#[tokio::test]
async fn a_get_is_not_allowed() {
    let url = serve().await;
    let response = reqwest::Client::new().get(&url).send().await.unwrap();
    assert_eq!(response.status(), 405);
}

#[tokio::test]
async fn an_origin_must_be_allowed() {
    let url = serve().await;
    let request = r#"{"jsonrpc":"2.0","id":1,"method":"echo","params":["hi"]}"#;
    let http = reqwest::Client::new();
    let from = |origin: &'static str| {
        http.post(&url)
            .header("content-type", "application/json")
            .header("origin", origin)
            .body(request)
            .send()
    };
    assert_eq!(from("https://evil.example").await.unwrap().status(), 403);
    assert_eq!(from("https://allowed.example").await.unwrap().status(), 200);
}
