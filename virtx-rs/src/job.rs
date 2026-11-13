//! A job's start, which answers with the job, and the log read from it once it has.

use std::{
    future::Future,
    mem,
    pin::Pin,
    task::{Context, Poll},
};

use futures_core::Stream;
use serde::de::DeserializeOwned;
use virtx_core::{ImageJobEntry, LogEntry, SandboxJobEntry};

use crate::{Call, Error, Events, ImageJob, SandboxJob};

/// What opens a job's log, given the job its start answered with.
type Open<J, E> = Box<dyn Fn(&J) -> Events<LogEntry<E>> + Send + Sync>;

/// A job being started, which answers with the job `J`: awaited or waited on as a [`Call`], or
/// read as its log of `E` entries with [`log`](Self::log). Dropping it before it answers cancels
/// the start; once started, the job runs until it ends or is removed.
#[must_use = "a call dropped is cancelled"]
pub struct StartedJob<J, E> {
    call: Call<J>,
    open: Open<J, E>,
}

impl<J, E> StartedJob<J, E> {
    pub(crate) fn new(call: Call<J>, open: Open<J, E>) -> StartedJob<J, E> {
        StartedJob { call, open }
    }

    /// Block this thread until the job has started, for a caller without an async runtime.
    pub fn wait(self) -> Result<J, Error> {
        self.call.wait()
    }
}

impl<J, E> StartedJob<J, E> {
    /// The job's log from its start, read once the job has started, until its `end` entry.
    /// A failed start is its one item. The job stays the log's, as [`JobLog::job`].
    pub fn log(self) -> JobLog<J, E> {
        JobLog {
            state: State::Starting(self),
            job: None,
        }
    }
}

impl StartedJob<ImageJob, ImageJobEntry> {
    /// The job once it has ended, its log read to its `end` entry: with its digest, or, for one
    /// that failed, its error as [`Error::Job`]. Its log is still read with
    /// [`ImageJob::log`].
    pub async fn finished(self) -> Result<ImageJob, Error> {
        finish(self.log(), ImageJob::ended).await
    }

    /// [`finished`](Self::finished), blocking this thread, for a caller without an async
    /// runtime.
    pub fn wait_finished(self) -> Result<ImageJob, Error> {
        wait_finish(self.log(), ImageJob::ended)
    }
}

impl StartedJob<SandboxJob, SandboxJobEntry> {
    /// The job once its process has ended, its log read to its `end` entry: with its exit
    /// code, which is not an error whatever it is. Its log is still read with
    /// [`SandboxJob::log`].
    pub async fn finished(self) -> Result<SandboxJob, Error> {
        finish(self.log(), SandboxJob::ended).await
    }

    /// [`finished`](Self::finished), blocking this thread, for a caller without an async
    /// runtime.
    pub fn wait_finished(self) -> Result<SandboxJob, Error> {
        wait_finish(self.log(), SandboxJob::ended)
    }
}

/// What a job is as an entry of its log says it ended, or `None` for an entry that is not its
/// end.
type Ended<J, E> = fn(&J, &E) -> Option<Result<J, Error>>;

/// The job `log` is of, as its `end` entry says it ended.
async fn finish<J, E: DeserializeOwned>(
    mut log: JobLog<J, E>,
    ended: Ended<J, E>,
) -> Result<J, Error> {
    while let Some(entry) = std::future::poll_fn(|cx| Pin::new(&mut log).poll_next(cx)).await {
        if let Some(job) = end(&log, &entry?.entry, ended) {
            return job;
        }
    }
    Err(unended())
}

/// [`finish`], blocking this thread.
fn wait_finish<J, E: DeserializeOwned>(
    mut log: JobLog<J, E>,
    ended: Ended<J, E>,
) -> Result<J, Error> {
    while let Some(entry) = log.wait_next() {
        if let Some(job) = end(&log, &entry?.entry, ended) {
            return job;
        }
    }
    Err(unended())
}

/// The job as `entry` of `log` says it ended, if it is the end.
fn end<J, E>(log: &JobLog<J, E>, entry: &E, ended: Ended<J, E>) -> Option<Result<J, Error>> {
    let job = log
        .job()
        .expect("an entry is read only once the job has started");
    ended(job, entry)
}

/// A log that ended with no `end` entry: the connection did, with the job still running.
fn unended() -> Error {
    Error::Call("the job's log ended before the job did".to_owned())
}

impl<J, E> Future for StartedJob<J, E> {
    type Output = Result<J, Error>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.call).poll(cx)
    }
}

/// A started job's log as a [`StartedJob`] gives it: each `LogEntry` of `E`, or the error it
/// failed with, polled as a `Stream` or waited on with [`wait_next`](Self::wait_next); and the
/// job `J` itself once started, as [`job`](Self::job).
/// Dropping it cancels a start not yet answered, and leaves a started job running.
#[must_use = "a log dropped is not read"]
pub struct JobLog<J, E> {
    state: State<J, E>,
    job: Option<J>,
}

enum State<J, E> {
    Starting(StartedJob<J, E>),
    Reading(Events<LogEntry<E>>),
    Done,
}

impl<J, E> JobLog<J, E> {
    /// The job its start answered with, once the first entry has been read: `None` before
    /// then, or if the start failed. Its id is what cancels, signals or writes to the job's
    /// stdin while its log is read.
    pub fn job(&self) -> Option<&J> {
        self.job.as_ref()
    }

    /// Keep the job `job` its start answered with, and read its log from here on.
    fn started(&mut self, open: &Open<J, E>, job: J) {
        self.state = State::Reading(open(&job));
        self.job = Some(job);
    }
}

impl<J, E> Unpin for JobLog<J, E> {}

impl<J, E: DeserializeOwned> JobLog<J, E> {
    /// Block this thread until the next entry, or `None` after the last, for a caller without
    /// an async runtime.
    pub fn wait_next(&mut self) -> Option<Result<LogEntry<E>, Error>> {
        if let State::Starting(_) = self.state {
            let State::Starting(StartedJob { call, open }) =
                mem::replace(&mut self.state, State::Done)
            else {
                unreachable!()
            };
            match call.wait() {
                Ok(job) => self.started(&open, job),
                Err(error) => return Some(Err(error)),
            }
        }
        match &mut self.state {
            State::Reading(events) => events.wait_next(),
            _ => None,
        }
    }
}

impl<J, E: DeserializeOwned> Stream for JobLog<J, E> {
    type Item = Result<LogEntry<E>, Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if let State::Starting(start) = &mut self.state {
            match Pin::new(&mut start.call).poll(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Ok(job)) => {
                    let State::Starting(start) = mem::replace(&mut self.state, State::Done) else {
                        unreachable!()
                    };
                    self.started(&start.open, job);
                }
                Poll::Ready(Err(error)) => {
                    self.state = State::Done;
                    return Poll::Ready(Some(Err(error)));
                }
            }
        }
        match &mut self.state {
            State::Reading(events) => Pin::new(events).poll_next(cx),
            _ => Poll::Ready(None),
        }
    }
}
