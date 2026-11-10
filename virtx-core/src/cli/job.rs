//! Following an image job: its log on stderr until its end, and Ctrl-C cancelling it.

use std::fmt;

use futures_util::StreamExt as _;

use crate::{Client, ImageJob, ImageJobEntry};

/// Ctrl-C interrupted the command, and what it was doing was undone, as said.
#[derive(Debug)]
pub(crate) struct Interrupted(pub(crate) &'static str);

impl fmt::Display for Interrupted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "interrupted; {}", self.0)
    }
}

impl std::error::Error for Interrupted {}

/// Follow `job` to its end and answer the digest it ended with, its log on stderr unless
/// `quiet`. Ctrl-C cancels it, since a job runs on without the reader that started it.
pub(crate) async fn follow(client: &Client, job: &ImageJob, quiet: bool) -> anyhow::Result<String> {
    tokio::select! {
        ended = drain(client, &job.id, quiet) => ended,
        _ = tokio::signal::ctrl_c() => {
            client.remove_image_job(&job.id).await?;
            Err(Interrupted("the job was cancelled").into())
        }
    }
}

async fn drain(client: &Client, id: &str, quiet: bool) -> anyhow::Result<String> {
    let mut log = client.image_job_log(id, None);
    while let Some(entry) = log.next().await {
        match entry?.entry {
            ImageJobEntry::Log(line) if !quiet => eprintln!("{line}"),
            ImageJobEntry::Truncated {} if !quiet => {
                eprintln!("(the log reached the server's limit; the rest is not kept)")
            }
            ImageJobEntry::End(end) => return ended(end.digest, end.error),
            _ => {}
        }
    }
    // The stream closed before its end: the job says how it stands.
    let job = client.get_image_job(id).await?;
    anyhow::ensure!(
        !job.running,
        "the job's log closed while it ran; it is {id}"
    );
    ended(job.digest, job.error)
}

fn ended(digest: Option<String>, error: Option<String>) -> anyhow::Result<String> {
    match (digest, error) {
        (_, Some(error)) => Err(anyhow::anyhow!(error)),
        (Some(digest), None) => Ok(digest),
        (None, None) => anyhow::bail!("the job ended without a digest"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_job_ended_answers_its_digest_or_why_it_failed() {
        let digest = || Some("sha256:0".to_owned());
        assert_eq!(ended(digest(), None).unwrap(), "sha256:0");
        // An error is how it ended, whatever digest it has.
        let failed = ended(digest(), Some("no space".into())).unwrap_err();
        assert_eq!(failed.to_string(), "no space");
        assert!(ended(None, None).is_err());
    }

    #[test]
    fn an_interruption_says_what_was_undone() {
        let interrupted = Interrupted("the job was cancelled");
        assert_eq!(
            interrupted.to_string(),
            "interrupted; the job was cancelled"
        );
    }
}
