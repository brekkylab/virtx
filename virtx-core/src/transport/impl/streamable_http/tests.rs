//! Both ends of Streamable HTTP, and the server against `PROTOCOL.md` by hand.

use std::sync::{Arc, Mutex};

use ciborium::{Value, cbor};
use futures_util::StreamExt;
use http::{Extensions, HeaderMap, StatusCode};
use tokio::net::TcpListener;

use super::*;
use crate::method::Request;
use crate::transport::{
    Dispatcher, Event, TransportClient,
    r#impl::{BoxFuture, frame::Parser},
    testing::methods,
};

/// A [`Server`] on localhost, serving `echo`, `count` (which sends progress before it
/// answers) and `session` (which answers the request's session id).
async fn serve() -> String {
    let server = Server::with_origins(
        Dispatcher::new(methods()),
        vec!["https://allowed.example".into()],
    );
    mount(server).await
}

/// The user a [`Tokens`] session was opened by.
#[derive(Clone)]
struct User(String);

/// `user`: answers the [`User`] of the request's session.
struct WhoAmI;

impl Request for WhoAmI {
    const NAME: &'static str = "user";
    type Args = [(); 0];
    type Output = String;
}

/// Sessions for a bearer token naming one of `users`, used by that token alone, and the ids
/// of those ended, in `closed`.
struct Tokens {
    users: Vec<&'static str>,
    closed: Arc<Mutex<Vec<String>>>,
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get("authorization")?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}

impl Sessions for Tokens {
    fn open<'a>(&'a self, headers: &'a HeaderMap) -> BoxFuture<'a, Result<Extensions, StatusCode>> {
        Box::pin(async move {
            let user = bearer(headers).ok_or(StatusCode::UNAUTHORIZED)?;
            if !self.users.contains(&user) {
                return Err(StatusCode::FORBIDDEN);
            }
            let mut extensions = Extensions::new();
            extensions.insert(User(user.to_owned()));
            Ok(extensions)
        })
    }

    fn check(&self, session: &Extensions, headers: &HeaderMap) -> Result<(), StatusCode> {
        let owner = &session.get::<User>().expect("opened with one").0;
        match bearer(headers) {
            Some(user) if user == owner => Ok(()),
            _ => Err(StatusCode::FORBIDDEN),
        }
    }

    fn close(&self, id: &SessionId, session: Extensions) {
        assert!(session.get::<User>().is_some());
        self.closed.lock().unwrap().push(id.0.clone());
    }
}

/// A [`Server`] on localhost as [`serve`]'s, with `user` too, its sessions [`Tokens`] for
/// `alice` and `bob`; and the ids of the sessions ended.
async fn serve_tokens() -> (String, Arc<Mutex<Vec<String>>>) {
    let closed = Arc::new(Mutex::new(Vec::new()));
    let mut methods = methods();
    methods.add::<WhoAmI, _, _>(|_, extensions| async move {
        Ok(extensions.get::<User>().expect("a user").0.clone())
    });
    let sessions = Tokens {
        users: vec!["alice", "bob"],
        closed: closed.clone(),
    };
    let server = Server::with_sessions(Dispatcher::new(methods), Vec::new(), sessions);
    (mount(server).await, closed)
}

/// `server` on localhost, at the URL answered.
async fn mount(server: Server) -> String {
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

/// A `POST` of `body` to `url` as `post`'s, with `token` as its bearer token.
async fn post_as(url: &str, body: &[u8], token: &str, session: Option<&str>) -> reqwest::Response {
    let mut request = reqwest::Client::new()
        .post(url)
        .header("content-type", CBOR)
        .header("accept", CBOR)
        .header("authorization", format!("Bearer {token}"))
        .body(body.to_owned());
    if let Some(id) = session {
        request = request.header(SESSION_HEADER, id);
    }
    request.send().await.unwrap()
}

fn user() -> Vec<u8> {
    encode(cbor!({"jsonrpc" => "2.0", "id" => 1, "method" => "user", "params" => []}).unwrap())
}

#[tokio::test]
async fn sessions_refuse_an_opening_and_open_none() {
    let (url, _) = serve_tokens().await;
    let anonymous = post(&url, &echo(), BOTH, None).await;
    assert_eq!(anonymous.status(), 401);
    assert!(!anonymous.headers().contains_key(SESSION_HEADER));
    let stranger = post_as(&url, &echo(), "mallory", None).await;
    assert_eq!(stranger.status(), 403);
    assert!(!stranger.headers().contains_key(SESSION_HEADER));
}

#[tokio::test]
async fn what_a_session_opened_with_reaches_its_every_method() {
    let (url, _) = serve_tokens().await;
    let opened = post_as(&url, &user(), "alice", None).await;
    let id = opened.headers()[SESSION_HEADER]
        .to_str()
        .unwrap()
        .to_owned();
    let answer = cbor!({"jsonrpc" => "2.0", "id" => 1, "result" => "alice"}).unwrap();
    assert_eq!(decode(&opened.bytes().await.unwrap()), answer);
    let again = post_as(&url, &user(), "alice", Some(&id)).await;
    assert_eq!(decode(&again.bytes().await.unwrap()), answer);
}

#[tokio::test]
async fn sessions_refuse_a_request_in_anothers_session() {
    let (url, closed) = serve_tokens().await;
    let opened = post_as(&url, &echo(), "alice", None).await;
    let id = opened.headers()[SESSION_HEADER]
        .to_str()
        .unwrap()
        .to_owned();
    assert_eq!(post_as(&url, &echo(), "bob", Some(&id)).await.status(), 403);
    assert_eq!(post(&url, &echo(), BOTH, Some(&id)).await.status(), 403);

    let http = reqwest::Client::new();
    let delete = |token: &str| {
        http.delete(&url)
            .header(SESSION_HEADER, &id)
            .header("authorization", format!("Bearer {token}"))
            .send()
    };
    assert_eq!(delete("bob").await.unwrap().status(), 403);
    assert!(closed.lock().unwrap().is_empty());
    assert_eq!(
        post_as(&url, &echo(), "alice", Some(&id)).await.status(),
        200
    );
    assert_eq!(delete("alice").await.unwrap().status(), 204);
    assert_eq!(*closed.lock().unwrap(), [id]);
}
