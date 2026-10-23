//! The stdio transport: messages as [`frame`]s on the server's stdin and stdout.

use std::{io, process::Stdio, time::Duration};

use http::Extensions;
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::mpsc,
    task::JoinSet,
};

use super::{
    BoxFuture, Incoming, Outgoing,
    frame::{self, Reader, frame},
};
use crate::transport::{Dispatcher, Notifier};

/// How long a server whose stdin is closed has to exit before it is killed.
pub const EXIT_GRACE: Duration = Duration::from_secs(5);

/// Start `command`, a [`std::process::Command`] or a tokio one, as the server, and the
/// transport over its stdin and stdout: whatever `command` set for those two is replaced. The
/// rest of it (arguments, environment, directory, stderr) is the caller's. Must be called
/// inside a tokio runtime.
pub fn spawn(command: impl Into<Command>) -> io::Result<(Sender, Receiver)> {
    let mut child = command
        .into()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let stdin = child.stdin.take().expect("piped");
    let stdout = child.stdout.take().expect("piped");
    let sender = Sender { stdin, child };
    let receiver = Receiver {
        frames: Reader::new(stdout),
    };
    Ok((sender, receiver))
}

/// The server's stdin, and the server, which dropping kills.
pub struct Sender {
    stdin: ChildStdin,
    child: Child,
}

impl Outgoing for Sender {
    fn send(&mut self, message: Vec<u8>) -> BoxFuture<'_, io::Result<()>> {
        Box::pin(async move {
            self.stdin.write_all(&frame(&message)).await?;
            self.stdin.flush().await
        })
    }

    /// Close the server's stdin, and kill it if it has not exited within [`EXIT_GRACE`].
    fn close(self: Box<Self>) -> BoxFuture<'static, ()> {
        let Sender { stdin, mut child } = *self;
        drop(stdin);
        Box::pin(async move {
            if tokio::time::timeout(EXIT_GRACE, child.wait())
                .await
                .is_err()
            {
                let _ = child.kill().await;
            }
        })
    }
}

pub struct Receiver {
    frames: Reader<ChildStdout>,
}

impl Incoming for Receiver {
    /// The next message: anything else on stdout is skipped.
    fn receive(&mut self) -> BoxFuture<'_, io::Result<Vec<u8>>> {
        Box::pin(async move {
            self.frames
                .next()
                .await?
                .ok_or(io::ErrorKind::UnexpectedEof.into())
        })
    }
}

/// Serve `dispatcher`'s methods over `input` and `output` (a server's stdin and stdout)
/// until `input` ends, then finish the requests in flight and return.
///
/// Requests run concurrently, and each response is written when it is ready. A method's
/// [`Notifier`] writes to `output` too.
pub async fn serve<I, O>(dispatcher: Dispatcher, input: I, output: O) -> io::Result<()>
where
    I: AsyncRead + Unpin,
    O: AsyncWrite + Unpin + Send + 'static,
{
    let (outbox, mut queued) = mpsc::unbounded_channel::<Vec<u8>>();
    // One writer, so two messages never interleave on `output`.
    let writer = tokio::spawn(async move {
        let mut output = output;
        while let Some(message) = queued.recv().await {
            output.write_all(&frame::frame(&message)).await?;
            output.flush().await?;
        }
        io::Result::Ok(())
    });

    let mut input = Reader::new(input);
    let mut calls = JoinSet::new();
    while let Some(message) = input.next().await? {
        let dispatcher = dispatcher.clone();
        let outbox = outbox.clone();
        calls.spawn(async move {
            let mut extensions = Extensions::new();
            extensions.insert(Notifier::new(outbox.clone()));
            if let Some(response) = dispatcher.call(&message, extensions).await {
                let _ = outbox.send(response);
            }
        });
    }
    calls.join_all().await;
    drop(outbox);
    writer.await.expect("the writer does not panic")
}

#[cfg(test)]
mod tests {
    use ciborium::cbor;
    use futures_util::StreamExt;
    use tokio::runtime::Runtime;

    use super::*;
    use crate::transport::{Event, TransportClient, testing::methods};

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
}
