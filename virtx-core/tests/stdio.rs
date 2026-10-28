//! Both ends of stdio, the server a child process: this test binary, run again as one.
//!
//! Without libtest's harness, which writes its own lines to stdout: over stdio, stdout is the
//! server's HTTP/2 connection and nothing else.

use std::time::Duration;

use futures_util::StreamExt;
use virtx_core::{
    Dispatcher, ErrorObject, Event, Methods, Notifier, Progress, Request, Server, StreamingRequest,
    TransportClient, serve_stdio,
};

/// Set in the environment of this binary when it runs as the server.
const SERVER: &str = "VIRTX_CORE_STDIO_SERVER";

/// `echo`: answers the text it is given.
struct Echo;

impl Request for Echo {
    const NAME: &'static str = "echo";
    type Args = (String,);
    type Output = String;
}

/// `count`: sends 1 up to the number it is given as its progress, then answers it.
struct Count;

impl Request for Count {
    const NAME: &'static str = "count";
    type Args = (u32,);
    type Output = u32;
}

impl StreamingRequest for Count {
    type Content = String;
}

fn methods() -> Methods {
    let mut methods = Methods::new();
    methods
        .add::<Echo, _, _>(|(text,), _| async move { Ok::<_, ErrorObject>(text) })
        .add::<Count, _, _>(|(to,), extensions| async move {
            let notifier = extensions.get::<Notifier>().expect("a notifier");
            for i in 1..=to {
                notifier.notify::<Count>(Progress {
                    progress: i.into(),
                    total: Some(to.into()),
                    content: None,
                });
            }
            Ok(to)
        });
    methods
}

fn main() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    if std::env::var_os(SERVER).is_some() {
        let server = Server::new(Dispatcher::new(methods()));
        let handle = move |request| {
            let server = server.clone();
            async move { server.handle(request).await }
        };
        let served = serve_stdio(handle, tokio::io::stdin(), tokio::io::stdout());
        runtime.block_on(served).unwrap();
        return;
    }
    runtime.block_on(a_child_process_answers_calls_and_their_progress());
    eprintln!("test result: ok");
}

async fn a_child_process_answers_calls_and_their_progress() {
    let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
    command.env(SERVER, "1");
    let client = TransportClient::spawn(command).unwrap();

    let answer: String = client.request("echo", ["hi"]).await.unwrap();
    assert_eq!(answer, "hi");

    let events: Vec<_> = client
        .request_with_progress::<u32, String>("count", [2])
        .map(Result::unwrap)
        .collect()
        .await;
    let count = |n| {
        Event::Progress(Progress {
            progress: n,
            total: Some(2),
            content: None,
        })
    };
    assert_eq!(events, [count(1), count(2), Event::Output(2)]);

    let started = std::time::Instant::now();
    client.close().await;
    // The server exits once its stdin closes, well before it would be killed.
    assert!(started.elapsed() < Duration::from_secs(5));
}
