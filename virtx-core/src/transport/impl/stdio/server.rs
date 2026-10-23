//! The server's end: serving a [`Dispatcher`] over its own stdin and stdout.

use std::io;

use http::Extensions;
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt},
    sync::mpsc,
    task::JoinSet,
};

use crate::transport::{
    Dispatcher, Notifier,
    r#impl::frame::{self, Reader},
};

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
