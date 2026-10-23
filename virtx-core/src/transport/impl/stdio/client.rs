//! The client's end: the server as a child process, and the transport over its stdin and
//! stdout.

use std::{io, process::Stdio, time::Duration};

use tokio::{
    io::AsyncWriteExt,
    process::{Child, ChildStdin, ChildStdout, Command},
};

use crate::transport::r#impl::{
    BoxFuture, Incoming, Outgoing,
    frame::{Reader, frame},
};

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
