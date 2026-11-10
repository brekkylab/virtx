//! A job's start, which answers with the job, the log read from it once it has, and the job
//! followed to its end.

use std::{
    future::Future,
    mem,
    pin::Pin,
    task::{Context, Poll},
};

use futures_core::Stream;
use serde::de::DeserializeOwned;
use virtx_core::LogEntry;

use crate::{Call, Error, Events, Source};

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

pub(crate) mod sealed {
    use serde::de::DeserializeOwned;

    use crate::Error;

    /// A job, as [`JobEnd`](super::JobEnd) follows it: its log's entries, and what it ends with.
    pub trait Ends {
        type Entry: DeserializeOwned;
        type Output;

        /// What the job ended with if `entry` is its `end` entry, printing it first if `print`.
        fn ended(entry: Self::Entry, print: bool) -> Option<Result<Self::Output, Error>>;
    }
}

impl<J: sealed::Ends> StartedJob<J, J::Entry> {
    /// Follow the job to its end once it has started, as its `complete` does.
    pub fn complete(self, print: bool) -> JobEnd<J> {
        JobEnd {
            log: self.log(),
            print,
        }
    }
}

/// A job `J` followed through its log to its end, as its `complete` gives it: awaited as a
/// `Future`, or waited on with [`wait`](Self::wait), it answers what the job ended with, or the
/// error it failed with. Each line is printed on the way if it was asked to. Dropping it stops
/// reading the log and leaves the job running.
#[must_use = "a job followed is not followed once dropped"]
pub struct JobEnd<J: sealed::Ends> {
    log: JobLog<J, J::Entry>,
    print: bool,
}

impl<J: sealed::Ends> JobEnd<J> {
    /// `log`, a started job's, followed to its end.
    pub(crate) fn new(log: Events<LogEntry<J::Entry>>, print: bool) -> JobEnd<J> {
        JobEnd {
            log: JobLog {
                state: State::Reading(log),
                job: None,
            },
            print,
        }
    }

    /// Block this thread until the job ends, for a caller without an async runtime.
    pub fn wait(mut self) -> Result<J::Output, Error> {
        while let Some(entry) = self.log.wait_next() {
            if let Some(ended) = J::ended(entry?.entry, self.print) {
                return ended;
            }
        }
        Err(removed())
    }
}

/// A log that closed before its `end` entry: the job was removed while it was read.
fn removed() -> Error {
    Error::Call(Source::Job, "the job was removed before it ended".into())
}

impl<J: sealed::Ends> Unpin for JobEnd<J> {}

impl<J: sealed::Ends> Future for JobEnd<J> {
    type Output = Result<J::Output, Error>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        loop {
            match Pin::new(&mut self.log).poll_next(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(None) => return Poll::Ready(Err(removed())),
                Poll::Ready(Some(Err(error))) => return Poll::Ready(Err(error)),
                Poll::Ready(Some(Ok(entry))) => {
                    if let Some(ended) = J::ended(entry.entry, self.print) {
                        return Poll::Ready(ended);
                    }
                }
            }
        }
    }
}
