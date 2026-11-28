//! The client's end of `/images/jobs/` and `/images/prune`.

use futures_util::{Stream, StreamExt};

use super::{
    ImageBuildArgs, ImageJob, ImageJobEnd, ImageJobEntry, ImagePruneOutput, ImagePullArgs,
    ImagePushArgs,
};
use http_over_x::segment;

use crate::{Client, Error, LogEntry};

/// The path of the image job `id`.
fn job(id: &str) -> String {
    format!("/images/jobs/{}", segment(id))
}

/// Every image job the server holds.
pub(crate) async fn list_job(client: &Client) -> Result<Vec<ImageJob>, Error> {
    Ok(client.wire().get("/images/jobs/", client.headers()).await?)
}

/// Start a pull as `args` says.
pub(crate) async fn pull(client: &Client, args: &ImagePullArgs) -> Result<ImageJob, Error> {
    Ok(client
        .wire()
        .post("/images/jobs/pull/", client.headers(), args)
        .await?)
}

/// Start a push as `args` says.
pub(crate) async fn push(client: &Client, args: &ImagePushArgs) -> Result<ImageJob, Error> {
    Ok(client
        .wire()
        .post("/images/jobs/push/", client.headers(), args)
        .await?)
}

/// Start a build as `args` says.
pub(crate) async fn build(client: &Client, args: &ImageBuildArgs) -> Result<ImageJob, Error> {
    Ok(client
        .wire()
        .post("/images/jobs/build/", client.headers(), args)
        .await?)
}

/// The image job `id`.
pub(crate) async fn get_job(client: &Client, id: &str) -> Result<ImageJob, Error> {
    Ok(client.wire().get(&job(id), client.headers()).await?)
}

/// Cancel the image job `id` if it runs, and forget it.
pub(crate) async fn remove_job(client: &Client, id: &str) -> Result<(), Error> {
    Ok(client.wire().delete(&job(id), client.headers()).await?)
}

/// The log of the image job `id`, after its `after`th entry if given, until its end: an
/// `end` entry if it succeeded, [`Error::Job`] in its place if it failed.
pub(crate) fn job_log<'a>(
    client: &'a Client,
    id: &str,
    after: Option<u64>,
) -> impl Stream<Item = Result<LogEntry<ImageJobEntry>, Error>> + Send + use<'a> {
    crate::log_entry::log(client, job(id), after).map(failed)
}

/// `entry`, or the error a failed job's `end` entry gives.
fn failed(entry: Result<LogEntry<ImageJobEntry>, Error>) -> Result<LogEntry<ImageJobEntry>, Error> {
    match entry? {
        LogEntry {
            entry:
                ImageJobEntry::End(ImageJobEnd {
                    error: Some(error), ..
                }),
            ..
        } => Err(Error::Job(error)),
        entry => Ok(entry),
    }
}

/// Delete each image no tag points at and no sandbox uses, and what only such images held.
pub(crate) async fn prune(client: &Client) -> Result<ImagePruneOutput, Error> {
    Ok(client
        .wire()
        .post("/images/prune", client.headers(), &serde_json::json!({}))
        .await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_end_is_its_error_and_every_other_entry_is_as_read() {
        let end = |digest: Option<&str>, error: Option<&str>| {
            Ok(LogEntry {
                id: 2,
                entry: ImageJobEntry::End(ImageJobEnd {
                    digest: digest.map(Into::into),
                    error: error.map(Into::into),
                }),
            })
        };
        let error = failed(end(None, Some("step 1 exited 3"))).unwrap_err();
        assert!(
            matches!(&error, Error::Job(why) if why == "step 1 exited 3"),
            "{error:?}"
        );
        assert_eq!(error.to_string(), "step 1 exited 3");

        let succeeded = failed(end(Some("sha256:0"), None)).unwrap();
        assert_eq!(succeeded, end(Some("sha256:0"), None).unwrap());
        let line = LogEntry {
            id: 1,
            entry: ImageJobEntry::Log("hi".into()),
        };
        assert_eq!(failed(Ok(line.clone())).unwrap(), line);
    }
}
