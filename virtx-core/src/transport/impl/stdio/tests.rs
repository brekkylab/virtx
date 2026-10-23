//! Both ends of stdio: a client of this test binary, run as a server.

use ciborium::cbor;
use futures_util::StreamExt;
use tokio::{process::Command, runtime::Runtime, task::JoinSet};

use super::*;
use crate::transport::{
    Dispatcher, Event, TransportClient, r#impl::frame::frame, testing::methods,
};

/// The methods every test server answers.
fn dispatcher() -> Dispatcher {
    Dispatcher::new(methods())
}

/// Set in the environment of the test binary run as [`server`].
const SERVER: &str = "VIRTX_CORE_PROTOCOL_STDIO_SERVER";

/// Serves [`dispatcher`] over this process's stdio when run as [`spawn`]'s server, and
/// does nothing otherwise. libtest writes its own lines to stdout too, which the client
/// skips, as they are outside any frame.
#[test]
#[ignore = "the server `spawn` runs, not a test"]
fn server() {
    if std::env::var_os(SERVER).is_none() {
        return;
    }
    Runtime::new()
        .unwrap()
        .block_on(serve(dispatcher(), tokio::io::stdin(), tokio::io::stdout()))
        .unwrap();
}

/// A session with [`server`], run from this test binary.
fn spawn() -> TransportClient {
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command.env(SERVER, "1").args([
        "--exact",
        "--ignored",
        "--nocapture",
        "transport::r#impl::stdio::tests::server",
    ]);
    let (outgoing, incoming) = super::spawn(command).unwrap();
    TransportClient::new(outgoing, incoming)
}

#[tokio::test]
async fn a_call_answers_over_the_child_process() {
    let session = spawn();
    let answer: String = session.request("echo", ["hi"]).await.unwrap();
    assert_eq!(answer, "hi");
    session.close().await;
}

#[tokio::test]
async fn progress_reaches_the_call_before_its_response() {
    let session = spawn();
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
async fn concurrent_calls_each_get_their_own_progress() {
    let session = std::sync::Arc::new(spawn());
    let mut calls = JoinSet::new();
    for to in 1..=10u32 {
        let session = std::sync::Arc::clone(&session);
        calls.spawn(async move {
            let events: Vec<_> = session
                .request_with_progress::<u32>("count", [to])
                .map(Result::unwrap)
                .collect()
                .await;
            (to, events)
        });
    }
    for (to, events) in calls.join_all().await {
        let expected: Vec<_> = (1..=to)
            .map(|i| Event::Progress(i.to_string()))
            .chain([Event::Output(to)])
            .collect();
        assert_eq!(events, expected);
    }
    let session = std::sync::Arc::into_inner(session).unwrap();
    session.close().await;
}

#[tokio::test]
async fn concurrent_calls_each_get_their_own_response() {
    let session = std::sync::Arc::new(spawn());
    let mut calls = JoinSet::new();
    for i in 0..20 {
        let session = std::sync::Arc::clone(&session);
        calls.spawn(async move {
            let answer: String = session.request("echo", [i.to_string()]).await.unwrap();
            (i, answer)
        });
    }
    for (i, answer) in calls.join_all().await {
        assert_eq!(answer, i.to_string());
    }
    let session = std::sync::Arc::into_inner(session).unwrap();
    session.close().await;
}

#[tokio::test]
async fn a_missing_method_is_an_error_not_an_end() {
    let session = spawn();
    let error = session
        .request::<String>("nope", [(); 0])
        .await
        .unwrap_err();
    assert!(error.to_string().contains("Method not found"), "{error}");
    let answer: String = session.request("echo", ["hi"]).await.unwrap();
    assert_eq!(answer, "hi");
    session.close().await;
}

#[tokio::test]
async fn serve_answers_each_frame_and_ends_with_its_input() {
    let message = |value: ciborium::Value| {
        let mut bytes = Vec::new();
        ciborium::into_writer(&value, &mut bytes).unwrap();
        frame(&bytes)
    };
    let mut input = message(
        cbor!({"jsonrpc" => "2.0", "id" => 1, "method" => "echo", "params" => ["a"]}).unwrap(),
    );
    input.extend(message(
        cbor!({"jsonrpc" => "2.0", "method" => "echo", "params" => ["b"]}).unwrap(),
    ));
    let (output, mut read) = tokio::io::duplex(4096);
    serve(dispatcher(), &input[..], output).await.unwrap();
    let mut written = Vec::new();
    tokio::io::AsyncReadExt::read_to_end(&mut read, &mut written)
        .await
        .unwrap();
    let answer = cbor!({"jsonrpc" => "2.0", "id" => 1, "result" => "a"}).unwrap();
    assert_eq!(written, message(answer));
}

#[cfg(unix)]
#[tokio::test]
async fn a_server_that_exits_ends_the_client() {
    let (outgoing, incoming) = super::spawn(Command::new("true")).unwrap();
    let session = TransportClient::new(outgoing, incoming);
    let error = session.request::<String>("echo", ["hi"]).await.unwrap_err();
    assert!(!session.is_connected(), "{error}");
    session.close().await;
}
