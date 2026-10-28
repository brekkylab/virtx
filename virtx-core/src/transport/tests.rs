//! The methods the tests serve, and both ends together: over HTTP, served by axum, and over
//! an in-memory HTTP/2 connection as stdio serves one; and the server against `PROTOCOL.md` by
//! hand.
//!
//! `image.build` sends `pulling <base>` as its progress, and fails for the base `missing`.
//! `sandbox.exec` runs everything as `echo`, sending its line as its progress.

use std::{sync::Arc, time::Duration};

use axum::{extract::State, middleware::Next, response::IntoResponse};
use futures_util::StreamExt;
use http::{HeaderMap, StatusCode};
use serde_json::{Value, json};
use sse_stream::SseStream;
use tokio::{net::TcpListener, sync::mpsc, task::JoinSet};

use super::{
    CancelScope, Dispatcher, ErrorObject, Methods, Notifier, Server, TransportClient, serve_stdio,
};
use crate::{
    image::{ImageBuild, ImageBuildOutput},
    method::{Event, Progress, Request, StreamingRequest},
    sandbox::{SandboxExec, SandboxExecOutput},
};

/// `echo`: answers the text it is given.
pub struct Echo;

impl Request for Echo {
    const NAME: &'static str = "echo";
    type Args = (String,);
    type Output = String;
}

/// `count`: sends 1 up to the number it is given as its progress, that number its total and
/// no content, then answers it.
pub struct Count;

impl Request for Count {
    const NAME: &'static str = "count";
    type Args = (u32,);
    type Output = u32;
}

impl StreamingRequest for Count {
    type Content = String;
}

/// The `n`th line of a log.
fn line(n: u64, line: String) -> Progress<String> {
    Progress {
        progress: n,
        total: None,
        content: Some(line),
    }
}

pub fn methods() -> Methods {
    let mut methods = Methods::new();
    methods
        .add::<Echo, _, _>(|(text,), _| async move { Ok(text) })
        .add::<Count, _, _>(|(to,), extensions| async move {
            let notifier = extensions.get::<Notifier>().expect("a notifier");
            for i in 1..=to {
                notifier.notify::<Count>(Progress {
                    progress: i.into(),
                    total: Some(to.into()),
                    content: None,
                });
                tokio::task::yield_now().await;
            }
            Ok(to)
        })
        .add::<ImageBuild, _, _>(|args, extensions| async move {
            let notifier = extensions.get::<Notifier>().expect("a notifier");
            let base = &args.recipe.base;
            notifier.notify::<ImageBuild>(line(1, format!("pulling {base}")));
            if base == "missing" {
                return Err(ErrorObject::new(1, format!("no image {base}")));
            }
            Ok(ImageBuildOutput {
                digest: "sha256:0".into(),
            })
        })
        .add::<SandboxExec, _, _>(|args, extensions| async move {
            let notifier = extensions.get::<Notifier>().expect("a notifier");
            notifier.notify::<SandboxExec>(line(1, args.cmd[1..].join(" ")));
            Ok(SandboxExecOutput { code: 0 })
        });
    methods
}

/// A client of `methods` over an in-memory HTTP/2 connection, served as stdio serves.
pub fn piped(methods: Methods) -> TransportClient {
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (input, output) = tokio::io::split(server);
    tokio::spawn(serve_stdio(Dispatcher::new(methods), input, output));
    TransportClient::over(client)
}

/// A client of [`methods`], served on this runtime over an in-memory pipe.
#[cfg(feature = "ffi")]
pub fn connect() -> TransportClient {
    piped(methods())
}

/// `user`: answers the [`User`] of the request.
struct WhoAmI;

impl Request for WhoAmI {
    const NAME: &'static str = "user";
    type Args = [(); 0];
    type Output = String;
}

/// `hang`: sends one progress, then never answers.
struct Hang;

impl Request for Hang {
    const NAME: &'static str = "hang";
    type Args = [(); 0];
    type Output = ();
}

impl StreamingRequest for Hang {
    type Content = String;
}

/// Says when it is dropped.
struct Dropped(mpsc::UnboundedSender<()>);

impl Drop for Dropped {
    fn drop(&mut self) {
        let _ = self.0.send(());
    }
}

/// [`methods`] and [`Hang`], which says on the receiver when a `hang` is dropped.
fn hanging() -> (Methods, mpsc::UnboundedReceiver<()>) {
    let (dropped, receiver) = mpsc::unbounded_channel();
    let mut methods = methods();
    methods.add::<Hang, _, _>(move |_, extensions| {
        let dropped = Dropped(dropped.clone());
        async move {
            let _dropped = dropped;
            let notifier = extensions.get::<Notifier>().expect("a notifier");
            notifier.notify::<Hang>(Progress {
                progress: 1,
                total: None,
                content: Some("hanging".into()),
            });
            std::future::pending::<Result<(), ErrorObject>>().await
        }
    });
    (methods, receiver)
}

/// The user a [`tokens`] request was let in as.
#[derive(Clone)]
struct User(String);

/// Lets in a request whose bearer token names one of `users`, as that [`User`] and in that
/// user's [`CancelScope`]. Middleware in front of a [`Server`], as a mounting server's would be.
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
    request.extensions_mut().insert(CancelScope(user.clone()));
    request.extensions_mut().insert(User(user));
    next.run(request).await
}

/// `methods` served at `/rpc` under the base URL this returns, behind `tokens` if given.
async fn serve(methods: Methods, tokens: Option<Tokens>) -> String {
    serve_at("/rpc", methods, tokens).await
}

/// `methods` served at `path`, and the server's base URL.
async fn serve_at(path: &str, methods: Methods, tokens: Option<Tokens>) -> String {
    let server = Server::new(Dispatcher::new(methods));
    let mut router = axum::Router::new().route(
        path,
        axum::routing::any(move |request: axum::extract::Request| {
            let server = server.clone();
            async move { server.handle(request).await }
        }),
    );
    if let Some(tokens) = tokens {
        router = router.layer(axum::middleware::from_fn_with_state(tokens, self::tokens));
    }
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await });
    url
}

/// A client of [`methods`] over each transport.
async fn clients() -> [TransportClient; 2] {
    let url = serve(methods(), None).await;
    [TransportClient::connect(&url).unwrap(), piped(methods())]
}

/// What `count` sends as its `n`th progress, counting to `to`.
fn count(n: u32, to: u32) -> Progress<String> {
    Progress {
        progress: n.into(),
        total: Some(to.into()),
        content: None,
    }
}

#[tokio::test]
async fn a_client_calls() {
    for client in clients().await {
        let answer: String = client.request("echo", ["hi"]).await.unwrap();
        assert_eq!(answer, "hi");
        client.close().await;
    }
}

#[tokio::test]
async fn a_client_receives_progress_before_the_output() {
    for client in clients().await {
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
}

#[tokio::test]
async fn concurrent_calls_each_get_their_own_progress_and_response() {
    for client in clients().await {
        let client = Arc::new(client);
        let mut calls = JoinSet::new();
        for to in 1..=10u32 {
            let client = Arc::clone(&client);
            calls.spawn(async move {
                let events: Vec<_> = client
                    .request_with_progress::<u32, String>("count", [to])
                    .map(Result::unwrap)
                    .collect()
                    .await;
                (to, events)
            });
        }
        for (to, events) in calls.join_all().await {
            let expected: Vec<_> = (1..=to)
                .map(|i| Event::Progress(count(i, to)))
                .chain([Event::Output(to)])
                .collect();
            assert_eq!(events, expected);
        }
        Arc::into_inner(client).unwrap().close().await;
    }
}

#[tokio::test]
async fn a_failed_call_is_an_error_not_an_end() {
    for client in clients().await {
        let error = client.request::<String>("nope", [(); 0]).await.unwrap_err();
        assert!(error.to_string().contains("Method not found"), "{error}");
        let answer: String = client.request("echo", ["hi"]).await.unwrap();
        assert_eq!(answer, "hi");
        client.close().await;
    }
}

#[tokio::test]
async fn dropping_a_call_cancels_it_on_the_server() {
    let (http, mut http_dropped) = hanging();
    let url = serve(http, None).await;
    let (piped_methods, mut piped_dropped) = hanging();
    let cases = [
        (TransportClient::connect(&url).unwrap(), &mut http_dropped),
        (piped(piped_methods), &mut piped_dropped),
    ];
    for (client, dropped) in cases {
        let mut hang = Box::pin(client.request_with_progress::<(), String>("hang", [(); 0]));
        assert!(matches!(hang.next().await, Some(Ok(Event::Progress(_)))));
        drop(hang);
        dropped.recv().await.expect("the method is dropped");
        client.close().await;
    }
}

#[tokio::test]
async fn a_failed_post_fails_its_call() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    drop(listener);
    let client = TransportClient::connect(&url).unwrap();
    for _ in 0..2 {
        assert!(client.request::<String>("echo", ["hi"]).await.is_err());
    }
}

#[tokio::test]
async fn an_http_error_fails_its_call_with_the_status() {
    let url = serve(methods(), Some(Tokens { users: vec![] })).await;
    let client = TransportClient::connect(&url).unwrap();
    let error = client.request::<String>("echo", ["hi"]).await.unwrap_err();
    assert!(error.to_string().contains("401"), "{error}");
}

#[test]
fn connect_refuses_anything_but_an_http_url() {
    for url in ["not a url", "file:///tmp/x"] {
        assert!(TransportClient::connect(url).is_err(), "{url}");
    }
}

#[tokio::test]
async fn rpc_is_under_the_base_url_with_or_without_its_slash() {
    let url = serve_at("/base/rpc", methods(), None).await;
    for base in [format!("{url}base"), format!("{url}base/")] {
        let client = TransportClient::connect(&base).unwrap();
        let answer: String = client.request("echo", ["hi"]).await.unwrap();
        assert_eq!(answer, "hi");
    }
}

#[cfg(unix)]
#[tokio::test]
async fn a_server_that_exits_fails_every_call() {
    let client = TransportClient::spawn(tokio::process::Command::new("true")).unwrap();
    for _ in 0..2 {
        assert!(client.request::<String>("echo", ["hi"]).await.is_err());
    }
    client.close().await;
}

#[tokio::test]
async fn serve_returns_once_its_client_is_gone() {
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (input, output) = tokio::io::split(server);
    let served = tokio::spawn(serve_stdio(Dispatcher::new(methods()), input, output));
    let client = TransportClient::over(client);
    let answer: String = client.request("echo", ["hi"]).await.unwrap();
    assert_eq!(answer, "hi");
    client.close().await;
    served.await.unwrap().unwrap();
}

/// `message` posted to `/rpc` at `url`, accepting `accept`.
async fn post(url: &str, accept: &str, message: Value) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{url}rpc"))
        .header("content-type", "application/json")
        .header("accept", accept)
        .body(message.to_string())
        .send()
        .await
        .unwrap()
}

const BOTH: &str = "application/json, text/event-stream";

#[tokio::test]
async fn a_request_without_notifications_answers_one_message() {
    let url = serve(methods(), None).await;
    let echo = json!({"jsonrpc": "2.0", "id": 1, "method": "echo", "params": ["hi"]});
    let response = post(&url, BOTH, echo).await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["content-type"], "application/json");
    let answer: Value = response.json().await.unwrap();
    assert_eq!(answer, json!({"jsonrpc": "2.0", "id": 1, "result": "hi"}));
}

#[tokio::test]
async fn a_request_with_notifications_answers_an_event_stream_ending_in_its_response() {
    let url = serve(methods(), None).await;
    let count = json!({"jsonrpc": "2.0", "id": 1, "method": "count", "params": [2]});
    let response = post(&url, BOTH, count).await;
    assert_eq!(response.headers()["content-type"], "text/event-stream");
    let body = response.bytes().await.unwrap();
    // One `data` line an event, and no other field.
    for event in String::from_utf8(body.to_vec())
        .unwrap()
        .split_terminator("\n\n")
    {
        assert!(
            event.starts_with("data: ") && !event.contains('\n'),
            "{event:?}"
        );
    }
    let events =
        SseStream::from_bytes_stream(futures_util::stream::iter([Ok::<_, std::io::Error>(body)]));
    let messages: Vec<Value> = events
        .map(|event| serde_json::from_str(&event.unwrap().data.unwrap()).unwrap())
        .collect()
        .await;
    let progress = |progress: u32| {
        json!({
            "jsonrpc": "2.0",
            "method": "notifications/progress",
            "params": {"requestId": 1, "progress": progress, "total": 2},
        })
    };
    assert_eq!(
        messages,
        [
            progress(1),
            progress(2),
            json!({"jsonrpc": "2.0", "id": 1, "result": 2}),
        ]
    );
}

#[tokio::test]
async fn a_client_that_takes_no_stream_gets_one_message() {
    let url = serve(methods(), None).await;
    let count = json!({"jsonrpc": "2.0", "id": 1, "method": "count", "params": [2]});
    let response = post(&url, "application/json", count).await;
    assert_eq!(response.headers()["content-type"], "application/json");
    let answer: Value = response.json().await.unwrap();
    assert_eq!(answer, json!({"jsonrpc": "2.0", "id": 1, "result": 2}));
}

#[tokio::test]
async fn a_notification_is_accepted() {
    let url = serve(methods(), None).await;
    let notification = json!({"jsonrpc": "2.0", "method": "echo", "params": ["hi"]});
    let response = post(&url, BOTH, notification).await;
    assert_eq!(response.status(), 202);
}

#[tokio::test]
async fn an_error_is_still_a_200() {
    let url = serve(methods(), None).await;
    let response = reqwest::Client::new()
        .post(format!("{url}rpc"))
        .header("content-type", "application/json")
        .header("accept", BOTH)
        .body("not json")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let answer: Value = response.json().await.unwrap();
    assert_eq!(answer["id"], Value::Null);
    assert_eq!(answer["error"]["code"], -32700);
}

#[tokio::test]
async fn only_a_post_is_allowed() {
    let url = serve(methods(), None).await;
    let http = reqwest::Client::new();
    for response in [
        http.get(format!("{url}rpc")).send().await.unwrap(),
        http.delete(format!("{url}rpc")).send().await.unwrap(),
    ] {
        assert_eq!(response.status(), 405);
        assert_eq!(response.headers()["allow"], "POST");
    }
}

#[tokio::test]
async fn middleware_refuses_a_request_before_it_reaches_a_method() {
    let tokens = Tokens {
        users: vec!["alice"],
    };
    let url = serve(methods(), Some(tokens)).await;
    let echo = json!({"jsonrpc": "2.0", "id": 1, "method": "echo", "params": ["hi"]});
    let http = reqwest::Client::new();
    let anonymous = http
        .post(format!("{url}rpc"))
        .header("content-type", "application/json")
        .header("accept", BOTH)
        .body(echo.to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(anonymous.status(), 401);
    let stranger = http
        .post(format!("{url}rpc"))
        .header("content-type", "application/json")
        .header("accept", BOTH)
        .header("authorization", "Bearer mallory")
        .body(echo.to_string())
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
    let tokens = Tokens {
        users: vec!["alice", "bob"],
    };
    let url = serve(methods, Some(tokens)).await;
    let user = json!({"jsonrpc": "2.0", "id": 1, "method": "user", "params": []});
    for name in ["alice", "bob"] {
        let response = reqwest::Client::new()
            .post(format!("{url}rpc"))
            .header("content-type", "application/json")
            .header("accept", BOTH)
            .header("authorization", format!("Bearer {name}"))
            .body(user.to_string())
            .send()
            .await
            .unwrap();
        let answer: Value = response.json().await.unwrap();
        assert_eq!(answer, json!({"jsonrpc": "2.0", "id": 1, "result": name}));
    }
}

/// `message` posted to `/rpc` at `url` as `user`, if any, accepting both media types.
async fn post_as(url: &str, user: Option<&str>, message: Value) -> reqwest::Response {
    let mut request = reqwest::Client::new()
        .post(format!("{url}rpc"))
        .header("content-type", "application/json")
        .header("accept", BOTH);
    if let Some(user) = user {
        request = request.header("authorization", format!("Bearer {user}"));
    }
    request.body(message.to_string()).send().await.unwrap()
}

/// A `notifications/cancelled` of the request `id`.
fn cancelled(id: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "notifications/cancelled",
        "params": {"requestId": id, "reason": "test"},
    })
}

fn hang(id: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": "hang", "params": []})
}

/// Whether `dropped` says a method was dropped within a moment.
async fn was_dropped(dropped: &mut mpsc::UnboundedReceiver<()>) -> bool {
    tokio::time::timeout(Duration::from_millis(200), dropped.recv())
        .await
        .is_ok()
}

#[tokio::test]
async fn a_cancellation_stops_its_request_and_ends_its_stream_without_a_response() {
    let (methods, mut dropped) = hanging();
    let url = serve(methods, None).await;
    let hanging = post_as(&url, None, hang("h")).await;
    assert_eq!(hanging.headers()["content-type"], "text/event-stream");

    let cancel = post_as(&url, None, cancelled("h")).await;
    assert_eq!(cancel.status(), 202);
    dropped.recv().await.expect("the method is dropped");

    let body = hanging.text().await.unwrap();
    assert!(body.contains("notifications/progress"), "{body}");
    assert!(
        !body.contains("\"result\"") && !body.contains("\"error\""),
        "{body}"
    );
}

#[tokio::test]
async fn a_cancelled_request_without_a_stream_is_answered_no_content() {
    let (methods, mut dropped) = hanging();
    let url = serve(methods, None).await;
    let hanging = tokio::spawn({
        let url = url.clone();
        async move { post(&url, "application/json", hang("h")).await }
    });
    // Until its method runs, there is no request to cancel.
    while post_as(&url, None, cancelled("h")).await.status() == 202 {
        if was_dropped(&mut dropped).await {
            break;
        }
    }
    assert_eq!(hanging.await.unwrap().status(), 204);
}

#[tokio::test]
async fn a_cancellation_stops_only_a_request_of_its_own_scope() {
    let (methods, mut dropped) = hanging();
    let tokens = Tokens {
        users: vec!["alice", "bob"],
    };
    let url = serve(methods, Some(tokens)).await;
    let hanging = post_as(&url, Some("alice"), hang("h")).await;

    assert_eq!(
        post_as(&url, Some("bob"), cancelled("h")).await.status(),
        202
    );
    assert!(!was_dropped(&mut dropped).await);

    assert_eq!(
        post_as(&url, Some("alice"), cancelled("h")).await.status(),
        202
    );
    dropped.recv().await.expect("the method is dropped");
    drop(hanging);
}

#[tokio::test]
async fn a_cancellation_stops_only_a_request_of_its_own_server() {
    let (methods, mut dropped) = hanging();
    let url = serve(methods, None).await;
    let other = serve(super::tests::methods(), None).await;
    let hanging = post_as(&url, None, hang("h")).await;

    assert_eq!(post_as(&other, None, cancelled("h")).await.status(), 202);
    assert!(!was_dropped(&mut dropped).await);
    drop(hanging);
}

#[tokio::test]
async fn a_second_request_with_an_id_in_flight_is_not_what_a_cancellation_stops() {
    let (methods, mut dropped) = hanging();
    let url = serve(methods, None).await;
    let first = post_as(&url, None, hang("h")).await;
    let second = post_as(&url, None, hang("h")).await;

    assert_eq!(post_as(&url, None, cancelled("h")).await.status(), 202);
    dropped.recv().await.expect("the first is dropped");
    assert!(first.text().await.unwrap().contains("hanging"));
    assert!(!was_dropped(&mut dropped).await);
    drop(second);
}
