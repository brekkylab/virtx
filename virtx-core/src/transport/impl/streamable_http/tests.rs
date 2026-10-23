//! Both ends of Streamable HTTP, and the server against `PROTOCOL.md` by hand.

use ciborium::{Value, cbor};
use tokio::net::TcpListener;

use super::*;
use futures_util::StreamExt;

use crate::transport::{
    Dispatcher, Event, TransportClient, r#impl::frame::Parser, testing::methods,
};

/// A [`Server`] on localhost, serving `echo`, `count` (which sends progress before it
/// answers) and `session` (which answers the request's session id).
async fn serve() -> String {
    let server = Server::with_origins(
        Dispatcher::new(methods()),
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

fn encode(message: Value) -> Vec<u8> {
    let mut bytes = Vec::new();
    ciborium::into_writer(&message, &mut bytes).unwrap();
    bytes
}

fn decode(bytes: &[u8]) -> Value {
    ciborium::from_reader(bytes).unwrap()
}

/// `echo`ing `hi`, as request 1.
fn echo() -> Vec<u8> {
    encode(cbor!({"jsonrpc" => "2.0", "id" => 1, "method" => "echo", "params" => ["hi"]}).unwrap())
}

/// `count`ing to 2, as request 1.
fn count() -> Vec<u8> {
    encode(cbor!({"jsonrpc" => "2.0", "id" => 1, "method" => "count", "params" => [2]}).unwrap())
}

/// A `POST` of `body` to `url`, accepting what `accept` says, in `session` if one is given.
async fn post(url: &str, body: &[u8], accept: &str, session: Option<&str>) -> reqwest::Response {
    let mut request = reqwest::Client::new()
        .post(url)
        .header("content-type", CBOR)
        .header("accept", accept)
        .body(body.to_owned());
    if let Some(id) = session {
        request = request.header(SESSION_HEADER, id);
    }
    request.send().await.unwrap()
}

/// A client of the server at `url`.
fn connect(url: &str) -> TransportClient {
    let (outgoing, incoming) = super::connect(url).unwrap();
    TransportClient::new(outgoing, incoming)
}

const BOTH: &str = "application/cbor, application/cbor-seq";

#[tokio::test]
async fn a_client_calls_and_its_session_carries_over() {
    let url = serve().await;
    let session = connect(&url);
    let answer: String = session.request("echo", ["hi"]).await.unwrap();
    assert_eq!(answer, "hi");
    let first: String = session.request("session", [(); 0]).await.unwrap();
    let second: String = session.request("session", [(); 0]).await.unwrap();
    assert_eq!(first, second);
    session.close().await;
}

#[tokio::test]
async fn a_client_receives_progress_over_the_stream() {
    let url = serve().await;
    let session = connect(&url);
    let events: Vec<_> = session
        .request_with_progress::<u32>("count", [3])
        .map(Result::unwrap)
        .collect()
        .await;
    assert_eq!(
        events,
        [
            Event::Progress("1".into()),
            Event::Progress("2".into()),
            Event::Progress("3".into()),
            Event::Output(3)
        ]
    );
    session.close().await;
}

#[tokio::test]
async fn a_failed_post_fails_its_call_and_not_the_client() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/rpc", listener.local_addr().unwrap());
    drop(listener);
    let session = connect(&url);
    for _ in 0..2 {
        let error = session.request::<String>("echo", ["hi"]).await.unwrap_err();
        assert!(session.is_connected(), "{error}");
    }
    session.close().await;
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
async fn a_first_post_opens_a_session_and_answers_one_message() {
    let url = serve().await;
    let response = post(&url, &echo(), BOTH, None).await;
    assert_eq!(response.status(), 200);
    assert!(response.headers().contains_key(SESSION_HEADER));
    assert_eq!(response.headers()["content-type"], CBOR);
    assert_eq!(
        decode(&response.bytes().await.unwrap()),
        cbor!({"jsonrpc" => "2.0", "id" => 1, "result" => "hi"}).unwrap()
    );
}

#[tokio::test]
async fn a_request_with_notifications_answers_a_stream_ending_in_its_response() {
    let url = serve().await;
    let response = post(&url, &count(), BOTH, None).await;
    assert_eq!(response.headers()["content-type"], CBOR_SEQ);
    let mut parser = Parser::default();
    parser.push(&response.bytes().await.unwrap());
    let messages: Vec<_> = std::iter::from_fn(|| parser.next_message())
        .map(|message| decode(&message))
        .collect();
    let progress = |message| {
        cbor!({
            "jsonrpc" => "2.0",
            "method" => "notifications/progress",
            "params" => {"requestId" => 1, "message" => message},
        })
        .unwrap()
    };
    assert_eq!(
        messages,
        [
            progress("1"),
            progress("2"),
            cbor!({"jsonrpc" => "2.0", "id" => 1, "result" => 2}).unwrap(),
        ]
    );
}

#[tokio::test]
async fn a_client_that_takes_no_stream_gets_one_message() {
    let url = serve().await;
    let response = post(&url, &count(), CBOR, None).await;
    assert_eq!(response.headers()["content-type"], CBOR);
    assert_eq!(
        decode(&response.bytes().await.unwrap()),
        cbor!({"jsonrpc" => "2.0", "id" => 1, "result" => 2}).unwrap()
    );
}

#[tokio::test]
async fn a_notification_is_accepted() {
    let url = serve().await;
    let notification =
        encode(cbor!({"jsonrpc" => "2.0", "method" => "echo", "params" => ["hi"]}).unwrap());
    assert_eq!(post(&url, &notification, BOTH, None).await.status(), 202);
}

#[tokio::test]
async fn a_session_ends_with_a_delete_and_is_unknown_after() {
    let url = serve().await;
    let request = echo();
    let opened = post(&url, &request, BOTH, None).await;
    let id = opened.headers()[SESSION_HEADER]
        .to_str()
        .unwrap()
        .to_owned();
    assert_eq!(post(&url, &request, BOTH, Some(&id)).await.status(), 200);

    let http = reqwest::Client::new();
    let delete = || http.delete(&url).header(SESSION_HEADER, &id).send();
    assert_eq!(delete().await.unwrap().status(), 204);
    assert_eq!(delete().await.unwrap().status(), 404);
    assert_eq!(post(&url, &request, BOTH, Some(&id)).await.status(), 404);
}

#[tokio::test]
async fn an_unknown_session_is_not_found() {
    let url = serve().await;
    let request = echo();
    assert_eq!(post(&url, &request, BOTH, Some("nope")).await.status(), 404);
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
    let request = echo();
    let http = reqwest::Client::new();
    let from = |origin: &'static str| {
        http.post(&url)
            .header("content-type", CBOR)
            .header("origin", origin)
            .body(request.clone())
            .send()
    };
    assert_eq!(from("https://evil.example").await.unwrap().status(), 403);
    assert_eq!(from("https://allowed.example").await.unwrap().status(), 200);
}
