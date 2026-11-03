//! A job's start, which answers with the job, and the log read from it once it has.

use std::{
    future::Future,
    mem,
    pin::Pin,
    task::{Context, Poll},
};

use futures_core::Stream;
use serde::de::DeserializeOwned;
use virtx_core::LogEntry;

use crate::{Call, Error, Events};

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

impl<J: 'static, E: 'static> StartedJob<J, E> {
    /// The job's log from its start, read once the job has started, until its `end` entry.
    /// A failed start is its one item.
    pub fn log(self) -> JobLog<E> {
        JobLog {
            state: State::Starting(Box::new(self)),
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
/// failed with, polled as a `Stream` or waited on with [`wait_next`](Self::wait_next).
/// Dropping it cancels a start not yet answered, and leaves a started job running.
#[must_use = "a log dropped is not read"]
pub struct JobLog<E> {
    state: State<E>,
}

enum State<E> {
    Starting(Box<dyn Start<E>>),
    Reading(Events<LogEntry<E>>),
    Done,
}

/// A `StartedJob` with its job type forgotten: what answers with the job's log.
trait Start<E>: Send + Sync {
    fn wait(self: Box<Self>) -> Result<Events<LogEntry<E>>, Error>;
    fn poll(&mut self, cx: &mut Context<'_>) -> Poll<Result<Events<LogEntry<E>>, Error>>;
}

impl<J, E> Start<E> for StartedJob<J, E> {
    fn wait(self: Box<Self>) -> Result<Events<LogEntry<E>>, Error> {
        let job = self.call.wait()?;
        Ok((self.open)(&job))
    }

    fn poll(&mut self, cx: &mut Context<'_>) -> Poll<Result<Events<LogEntry<E>>, Error>> {
        Pin::new(&mut self.call)
            .poll(cx)
            .map(|job| job.map(|job| (self.open)(&job)))
    }
}

impl<E> Unpin for JobLog<E> {}

impl<E: DeserializeOwned> JobLog<E> {
    /// Block this thread until the next entry, or `None` after the last, for a caller without
    /// an async runtime.
    pub fn wait_next(&mut self) -> Option<Result<LogEntry<E>, Error>> {
        self.state = match mem::replace(&mut self.state, State::Done) {
            State::Starting(start) => match start.wait() {
                Ok(events) => State::Reading(events),
                Err(error) => return Some(Err(error)),
            },
            state => state,
        };
        match &mut self.state {
            State::Reading(events) => events.wait_next(),
            _ => None,
        }
    }
}

impl<E: DeserializeOwned> Stream for JobLog<E> {
    type Item = Result<LogEntry<E>, Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if let State::Starting(start) = &mut self.state {
            match start.poll(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Ok(events)) => self.state = State::Reading(events),
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
