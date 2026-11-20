use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use tokio::sync::oneshot;
use virtx_core::{Log, SandboxJob, SandboxJobEnd, SandboxJobEndReason, SandboxJobEntry};

use super::sandbox::LOG_LIMIT;
use crate::machine::wire::Taken;

/// A process in a sandbox, as it is answered, and its log.
pub struct Job {
    job: Mutex<SandboxJob>,
    log: Arc<Log<SandboxJobEntry>>,
    /// When it ended, once it has: of the ended jobs a sandbox keeps, the one that ended first
    /// goes first.
    ended_at: Mutex<Option<Instant>>,
    /// Its stdin, and the writes sent to it not yet written.
    stdin: Mutex<Stdin>,
    /// Held while a write to its stdin is under way, so they go one at a time.
    writing: tokio::sync::Mutex<()>,
}

/// A job's stdin: whether it takes writes still, whether the guest has said it gone, the number
/// the next write is sent under, and each write sent and not yet answered, by that number:
/// whether it was `eof`, and who waits.
#[derive(Default)]
struct Stdin {
    open: bool,
    gone: bool,
    next: u64,
    waiting: HashMap<u64, (bool, oneshot::Sender<Written>)>,
}

/// What the guest answered a write to a job's stdin: how many bytes of its data went in, and
/// what became of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Written {
    pub(super) bytes: u64,
    pub(super) taken: Taken,
}

impl Job {
    /// A job for `job`, its stdin open if `stdin`.
    pub(super) fn new(job: SandboxJob, stdin: bool) -> Arc<Job> {
        Arc::new(Job {
            job: Mutex::new(job),
            log: Log::new(LOG_LIMIT, SandboxJobEntry::Truncated {}),
            stdin: Mutex::new(Stdin {
                open: stdin,
                ..Stdin::default()
            }),
            writing: tokio::sync::Mutex::new(()),
            ended_at: Mutex::new(None),
        })
    }

    /// `job`, as a sandbox kept past its process had it, known again: ended as it ended, or,
    /// one that ran then, as killed for its server's restart. Its log is its end alone.
    pub(super) fn kept(mut job: SandboxJob) -> Arc<Job> {
        if job.running {
            (job.running, job.code) = (false, Some(128 + super::sandbox::SIGKILL));
            job.reason = Some(SandboxJobEndReason::Restarted);
        }
        let end = SandboxJobEnd {
            code: job.code.unwrap_or(128 + super::sandbox::SIGKILL),
            reason: job.reason,
        };
        let kept = Job::new(job, false);
        kept.log.end(SandboxJobEntry::End(end), || {
            *kept.ended_at.lock().unwrap() = Some(Instant::now());
        });
        kept
    }

    /// The job as it is now.
    pub fn get(&self) -> SandboxJob {
        self.job.lock().unwrap().clone()
    }

    /// Its output, and its end once it has ended.
    pub fn log(&self) -> &Arc<Log<SandboxJobEntry>> {
        &self.log
    }

    pub(super) fn write(&self, entry: SandboxJobEntry) {
        self.log.push(entry);
    }

    /// End it with `code`, if it runs still. What its stdin was sent and has not taken never
    /// will be: whoever waits on it is told so.
    pub(super) fn end(&self, code: i32) {
        *self.stdin.lock().unwrap() = Stdin::default();
        self.log
            .end(SandboxJobEntry::End(SandboxJobEnd::exited(code)), || {
                let mut job = self.job.lock().unwrap();
                (job.running, job.code) = (false, Some(code));
                *self.ended_at.lock().unwrap() = Some(Instant::now());
            });
    }

    /// When it ended, once it has.
    pub(super) fn ended_at(&self) -> Option<Instant> {
        *self.ended_at.lock().unwrap()
    }

    pub(super) fn remove(&self) {
        self.log.remove();
    }

    /// Its pid, and whether it runs still.
    pub(super) fn pid(&self) -> (u32, bool) {
        let job = self.job.lock().unwrap();
        (job.pid, job.running)
    }

    /// Send what `send` sends to its stdin, if it is open, under the number it is given, closing
    /// it if `eof`: done once the guest answers it [written](Job::written), which is once it is
    /// in the process's stdin or never will be. One write at a time. `None` for a stdin closed,
    /// or never open, and for a write its process went away from before an answer.
    pub(super) async fn write_stdin(
        &self,
        eof: bool,
        send: impl FnOnce(u64) -> Result<(), super::Error>,
    ) -> Result<Option<Written>, super::Error> {
        let _turn = self.writing.lock().await;
        let written = {
            // Sent and waited for under one lock, so its answer finds it waiting.
            let mut stdin = self.stdin.lock().unwrap();
            if !stdin.open {
                return Ok(None);
            }
            let seq = stdin.next;
            send(seq)?;
            stdin.next += 1;
            let (told, written) = oneshot::channel();
            stdin.waiting.insert(seq, (eof, told));
            stdin.open = !eof;
            written
        };
        Ok(written.await.ok())
    }

    /// The guest has answered the write `seq` to its stdin. One [`Closed`](Taken::Closed)
    /// leaves its stdin closed: its reader is gone. One [`Full`](Taken::Full) was not taken, so
    /// its `eof`, if it had one, did not close it, unless another write found it gone.
    pub(super) fn written(&self, seq: u64, written: Written) {
        let mut stdin = self.stdin.lock().unwrap();
        let Some((eof, told)) = stdin.waiting.remove(&seq) else {
            return;
        };
        match written.taken {
            Taken::Whole => {}
            Taken::Closed => (stdin.open, stdin.gone) = (false, true),
            Taken::Full => stdin.open |= eof && !stdin.gone,
        }
        let _ = told.send(written);
    }
}
