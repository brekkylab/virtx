//! The stdio transport: one message per line on the server's stdin and stdout.

use std::{io, process::Stdio, time::Duration};

use bytes::Bytes;
use jsonrpsee::{
    Extensions,
    core::client::{ReceivedMessage, TransportReceiverT, TransportSenderT},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, Lines},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::mpsc,
    task::JoinSet,
};

use crate::transport::{Dispatcher, Notifier, dispatch::Answer};

/// How long [`TransportClient::close`](crate::transport::TransportClient::close) waits for the
/// server to exit once its stdin is closed.
pub const EXIT_GRACE: Duration = Duration::from_secs(5);

/// Start `command` with its stdin and stdout piped, and the transport over them.
pub(super) fn spawn(mut command: Command) -> io::Result<(Sender, Receiver, Child)> {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let stdin = child.stdin.take().expect("piped");
    let stdout = child.stdout.take().expect("piped");
    let sender = Sender { stdin: Some(stdin) };
    let receiver = Receiver {
        lines: BufReader::new(stdout).lines(),
    };
    Ok((sender, receiver, child))
}

/// Wait for `child`, whose stdin is closed, to exit, and kill it if it has not within
/// [`EXIT_GRACE`].
pub(super) async fn close(mut child: Child) {
    if tokio::time::timeout(EXIT_GRACE, child.wait())
        .await
        .is_err()
    {
        let _ = child.kill().await;
    }
}

pub(super) struct Sender {
    /// `None` once closed.
    stdin: Option<ChildStdin>,
}

impl TransportSenderT for Sender {
    type Error = io::Error;

    async fn send(&mut self, message: String) -> io::Result<()> {
        let stdin = self.stdin.as_mut().ok_or(io::ErrorKind::BrokenPipe)?;
        stdin.write_all(message.as_bytes()).await?;
        stdin.write_all(b"\n").await?;
        stdin.flush().await
    }

    async fn close(&mut self) -> io::Result<()> {
        self.stdin = None;
        Ok(())
    }
}

pub(super) struct Receiver {
    lines: Lines<BufReader<ChildStdout>>,
}

impl TransportReceiverT for Receiver {
    type Error = io::Error;

    /// The next line that is a message. Anything else on stdout is skipped: jsonrpsee would
    /// end the session over a line it cannot parse.
    async fn receive(&mut self) -> io::Result<ReceivedMessage> {
        loop {
            match self.lines.next_line().await? {
                Some(line) if line.starts_with('{') => return Ok(ReceivedMessage::Text(line)),
                Some(_) => continue,
                None => return Err(io::ErrorKind::UnexpectedEof.into()),
            }
        }
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
    let (outbox, mut queued) = mpsc::unbounded_channel::<String>();
    // One writer, so two messages never interleave on `output`.
    let writer = tokio::spawn(async move {
        let mut output = output;
        while let Some(message) = queued.recv().await {
            output.write_all(message.as_bytes()).await?;
            output.write_all(b"\n").await?;
            output.flush().await?;
        }
        io::Result::Ok(())
    });

    let mut lines = BufReader::new(input).lines();
    let mut calls = JoinSet::new();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let dispatcher = dispatcher.clone();
        let outbox = outbox.clone();
        calls.spawn(async move {
            let mut extensions = Extensions::new();
            extensions.insert(Notifier::new(outbox.clone()));
            match dispatcher.call(Bytes::from(line), extensions).await {
                Answer::Response(response) | Answer::Refused(_, response)
                    if !response.is_empty() =>
                {
                    let _ = outbox.send(String::from_utf8_lossy(&response).into_owned());
                }
                _ => {}
            }
        });
    }
    calls.join_all().await;
    drop(outbox);
    writer.await.expect("the writer does not panic")
}

#[cfg(test)]
mod tests {
    use jsonrpsee::{RpcModule, rpc_params};

    use crate::transport::{Transport, TransportClient};
    use tokio::runtime::Runtime;

    use super::*;

    /// The methods every test server answers.
    fn dispatcher() -> Dispatcher {
        let mut module = RpcModule::new(());
        module
            .register_method("echo", |params, _, _| params.one::<String>())
            .unwrap();
        module
            .register_method("count", |params, _, extensions| {
                let to: u32 = params.one()?;
                let notifier = extensions.get::<Notifier>().expect("a notifier");
                for i in 1..=to {
                    notifier.notify("counted", i);
                }
                jsonrpsee::core::RpcResult::Ok(to)
            })
            .unwrap();
        Dispatcher::new(module)
    }

    /// Set in the environment of the test binary run as [`server`].
    const SERVER: &str = "VIRTX_CORE_PROTOCOL_STDIO_SERVER";

    /// Serves [`dispatcher`] over this process's stdio when run as [`spawn`]'s server, and
    /// does nothing otherwise. libtest writes its own lines to stdout too, which the client
    /// skips.
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
        let (sender, receiver, transport) = Transport::spawn(command).unwrap();
        TransportClient::new(sender, receiver, transport)
    }

    #[tokio::test]
    async fn a_call_answers_over_the_child_process() {
        let session = spawn();
        let answer: String = session.request("echo", rpc_params!["hi"]).await.unwrap();
        assert_eq!(answer, "hi");
        session.close().await;
    }

    #[tokio::test]
    async fn notifications_reach_the_client_before_the_response() {
        let session = spawn();
        let mut counted = session.notifications::<u32>("counted").await.unwrap();
        let answer: u32 = session.request("count", rpc_params![3]).await.unwrap();
        assert_eq!(answer, 3);
        for i in 1..=3 {
            assert_eq!(counted.next().await.unwrap().unwrap(), i);
        }
        session.close().await;
    }

    #[tokio::test]
    async fn a_missing_method_is_an_error_not_an_end() {
        let session = spawn();
        let error = session
            .request::<String>("nope", rpc_params![])
            .await
            .unwrap_err();
        assert!(error.to_string().contains("Method not found"), "{error}");
        let answer: String = session.request("echo", rpc_params!["hi"]).await.unwrap();
        assert_eq!(answer, "hi");
        session.close().await;
    }

    #[tokio::test]
    async fn serve_answers_each_line_and_ends_with_its_input() {
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"echo","params":["a"]}"#,
            "\n\n",
            r#"{"jsonrpc":"2.0","method":"echo","params":["b"]}"#,
            "\n",
        );
        let (output, mut read) = tokio::io::duplex(4096);
        serve(dispatcher(), input.as_bytes(), output).await.unwrap();
        let mut written = String::new();
        tokio::io::AsyncReadExt::read_to_string(&mut read, &mut written)
            .await
            .unwrap();
        assert_eq!(written, "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":\"a\"}\n");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_server_that_exits_ends_the_client() {
        let (sender, receiver, transport) = Transport::spawn(Command::new("true")).unwrap();
        let session = TransportClient::new(sender, receiver, transport);
        let error = session
            .request::<String>("echo", rpc_params!["hi"])
            .await
            .unwrap_err();
        assert!(!session.is_connected(), "{error}");
        session.close().await;
    }
}
