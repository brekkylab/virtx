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

#[cfg(all(test, feature = "testing"))]
pub(crate) mod tests {
    use crate::{Auth, Recipe};

    use super::*;

    /// The test server, served over HTTP on loopback, and its base URL.
    pub(crate) async fn serve() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, crate::testing::router()).await });
        url
    }

    /// A client of a test server of its own.
    pub(crate) async fn client() -> Client {
        Client::connect(&serve().await, Auth::None, http::HeaderMap::new()).unwrap()
    }

    /// Every tag on the server's images, with the digest it points at.
    pub(crate) async fn tags(client: &Client) -> std::collections::BTreeMap<String, String> {
        let listed = client.list_image().await.unwrap();
        listed
            .into_iter()
            .map(|image| (image.reference, image.digest))
            .collect()
    }

    #[tokio::test]
    async fn a_job_answers_the_digest_it_ended_with() {
        let client = client().await;
        let job = client
            .build_image(Recipe::new("alpine:3.20"), "jq:latest")
            .await
            .unwrap();
        let digest = follow(&client, &job, true).await.unwrap();
        let listed = client.list_image().await.unwrap();
        assert!(
            listed
                .iter()
                .any(|image| image.reference == "jq:latest" && image.digest == digest),
            "{listed:?}"
        );
    }

    #[tokio::test]
    async fn a_failed_job_fails_with_its_error() {
        let client = client().await;
        let job = client
            .build_image(Recipe::new("missing"), "jq:latest")
            .await
            .unwrap();
        let error = follow(&client, &job, true).await.unwrap_err();
        assert!(!error.is::<Interrupted>());
        assert!(!error.to_string().is_empty());
    }
}
