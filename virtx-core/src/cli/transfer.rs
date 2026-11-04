//! Following a transfer between the server and a file here: each blob's line on stderr, and
//! Ctrl-C dropping it, which leaves nothing done.

use crate::{Error, Event};
use futures_util::{Stream, StreamExt as _};

use crate::cli::job::Interrupted;

/// Drive `stream` to its output, its progress on stderr unless `quiet`. Ctrl-C drops it, and
/// fails with `undone`, what dropping it leaves.
pub(crate) async fn follow<R>(
    stream: impl Stream<Item = Result<Event<R, String>, Error>> + Unpin,
    quiet: bool,
    undone: &'static str,
) -> anyhow::Result<R> {
    tokio::select! {
        output = drain(stream, quiet) => output,
        _ = tokio::signal::ctrl_c() => Err(Interrupted(undone).into()),
    }
}

async fn drain<R>(
    mut stream: impl Stream<Item = Result<Event<R, String>, Error>> + Unpin,
    quiet: bool,
) -> anyhow::Result<R> {
    while let Some(event) = stream.next().await {
        match event? {
            Event::Output(output) => return Ok(output),
            Event::Progress(progress) if !quiet => {
                let count = match progress.total {
                    Some(total) => format!("{}/{total}", progress.progress),
                    None => progress.progress.to_string(),
                };
                eprintln!("[{count}] {}", progress.content.unwrap_or_default());
            }
            Event::Progress(_) => {}
        }
    }
    anyhow::bail!("the transfer ended without saying what it did")
}
