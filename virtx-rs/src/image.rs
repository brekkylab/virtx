//! An image job, and the client its requests go through.

use virtx_core::{ImageJob as ImageJobInfo, ImageJobEntry, LogEntry};

use crate::{Call, Client, Error, Events, JobEnd, Source};

/// A build, pull or push the server runs, as starting it or asking for it answered, and the
/// client that reaches it: what reads its log.
///
/// Cloning holds the same client once more. Dropping it leaves the job as it is; it goes with
/// [`remove`](Self::remove).
#[derive(Clone)]
pub struct ImageJob {
    client: Client,
    info: ImageJobInfo,
}

impl ImageJob {
    pub(crate) fn new(client: Client, info: ImageJobInfo) -> ImageJob {
        ImageJob { client, info }
    }

    /// The client its requests go through.
    pub fn client(&self) -> &Client {
        &self.client
    }

    pub fn id(&self) -> &str {
        &self.info.id
    }

    /// What the server answered about it: what it does, its reference, and whether and how it
    /// had ended, as it was then.
    pub fn info(&self) -> &ImageJobInfo {
        &self.info
    }

    /// It as the server has it now.
    pub fn get(&self) -> Call<ImageJob> {
        self.client.get_image_job(&self.info.id)
    }

    /// Its log, from its start or after its `after`th entry, until its `end` entry, or the
    /// error in its place if the job failed. Dropping it leaves the job as it is.
    pub fn log(&self, after: Option<u64>) -> Events<LogEntry<ImageJobEntry>> {
        self.client.image_job_log(&self.info.id, after)
    }

    /// Follow it to its end, printing each line of its log to stdout if `print`: the digest
    /// it ended with, or the error it failed with, of [`Source::Job`].
    pub fn complete(&self, print: bool) -> JobEnd<ImageJobEntry, String> {
        JobEnd::new(self.log(None), print, |entry, print| match entry {
            ImageJobEntry::Log(line) => {
                if print {
                    println!("{line}");
                }
                None
            }
            ImageJobEntry::Truncated {} => None,
            ImageJobEntry::End(end) => Some(end.digest.ok_or_else(|| {
                Error::Call(Source::Server, "the job ended without a digest".into())
            })),
        })
    }

    /// Cancel it if it runs, and forget it.
    pub fn remove(&self) -> Call<()> {
        self.client.remove_image_job(&self.info.id)
    }
}

impl std::fmt::Debug for ImageJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("ImageJob").field(&self.info).finish()
    }
}
