use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::{oneshot, watch};
use virtx_core::{Log, SandboxJob, SandboxJobEnd, SandboxJobEndReason, SandboxJobEntry};

use super::sandbox::{LOG_LIMIT, SIGKILL};

/// A process in a sandbox, as it is answered, and its log.
pub struct Job {
    job: Mutex<SandboxJob>,
    log: Arc<Log<SandboxJobEntry>>,
    /// Its code, once it has ended.
    code: watch::Sender<Option<i32>>,
    /// When it ended, once it has: the oldest ended go first ([`super::sandbox::KEEP_ENDED`]).
    ended_at: Mutex<Option<std::time::Instant>>,
    /// Why the server kills it, once it has set out to: what its end says if it is killed.
    killing: Mutex<Option<SandboxJobEndReason>>,
    /// Its stdin, and the writes sent to it not yet written.
    stdin: Mutex<Stdin>,
    /// Held while a write to its stdin is under way, so they go one at a time.
    writing: tokio::sync::Mutex<()>,
}

/// A job's stdin: whether it takes writes still, the number the next write is sent under, and
/// who waits for each sent to be answered, by that number.
#[derive(Default)]
struct Stdin {
    open: bool,
    next: u64,
    waiting: HashMap<u64, oneshot::Sender<Written>>,
}

/// What the guest answered a write to a job's stdin: how many bytes of its data went in, and
/// whether all of it did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Written {
    pub(super) bytes: u64,
    pub(super) whole: bool,
}

impl Job {
    /// A job for `job`, its stdin open if `stdin`.
    pub(super) fn new(job: SandboxJob, stdin: bool) -> Arc<Job> {
        Arc::new(Job {
            job: Mutex::new(job),
            log: Log::new(LOG_LIMIT, SandboxJobEntry::Truncated {}),
            code: watch::channel(None).0,
            ended_at: Mutex::new(None),
            killing: Mutex::new(None),
            stdin: Mutex::new(Stdin {
                open: stdin,
                ..Stdin::default()
            }),
            writing: tokio::sync::Mutex::new(()),
        })
    }

    /// `job`, as its sandbox was last kept, known again by a server started again over it:
    /// ended as it ended, or -- one that ran then -- killed by its server's restart. Its log is
    /// its end alone.
    pub(super) fn kept(mut job: SandboxJob) -> Arc<Job> {
        if job.running || job.code.is_none() {
            job.code = Some(128 + SIGKILL);
            job.reason = Some(SandboxJobEndReason::Restarted);
        }
        job.running = false;
        let end = SandboxJobEnd {
            code: job.code.unwrap_or(128 + SIGKILL),
            reason: job.reason,
        };
        let kept = Job::new(job, false);
        kept.end_as(end);
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

    /// How it ended, once it has: its exit code, `128 + signal` for one a signal ended.
    pub async fn ended(&self) -> i32 {
        let mut code = self.code.subscribe();
        match code.wait_for(Option::is_some).await {
            Ok(code) => code.unwrap_or(128 + super::sandbox::SIGKILL),
            Err(_) => 128 + super::sandbox::SIGKILL,
        }
    }

    pub(super) fn write(&self, entry: SandboxJobEntry) {
        self.log.push(entry);
    }

    /// The server kills it for `reason`, if it runs still: what its end says once a `SIGKILL`
    /// ends it, unless it is killed for another reason first.
    pub(super) fn killing(&self, reason: SandboxJobEndReason) {
        let running = self.job.lock().unwrap().running;
        let mut killing = self.killing.lock().unwrap();
        if running && killing.is_none() {
            *killing = Some(reason);
        }
    }

    /// End it with `code`, as its process ended, if it runs still: killed, for why the server
    /// killed it, if it did.
    pub(super) fn end(&self, code: i32) {
        let killed = code == 128 + SIGKILL;
        let reason = killed.then(|| *self.killing.lock().unwrap()).flatten();
        self.end_as(SandboxJobEnd { code, reason });
    }

    /// End it now, if it runs still, as killed for `reason`, or for the reason it is being
    /// killed for already: whatever its process does after is not its job's.
    pub(super) fn cut(&self, reason: SandboxJobEndReason) {
        let reason = self.killing.lock().unwrap().unwrap_or(reason);
        self.end_as(SandboxJobEnd {
            code: 128 + SIGKILL,
            reason: Some(reason),
        });
    }

    /// End it as `end` says, if it runs still. What its stdin was sent and has not taken never
    /// will be: whoever waits on it is told so.
    fn end_as(&self, end: SandboxJobEnd) {
        *self.stdin.lock().unwrap() = Stdin::default();
        let (code, reason) = (end.code, end.reason);
        let ended = self.log.end(SandboxJobEntry::End(end), || {
            let mut job = self.job.lock().unwrap();
            (job.running, job.code, job.reason) = (false, Some(code), reason);
        });
        if ended {
            *self.ended_at.lock().unwrap() = Some(std::time::Instant::now());
            self.code.send_replace(Some(code));
        }
    }

    /// When it ended, once it has.
    pub(super) fn ended_at(&self) -> Option<std::time::Instant> {
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
    /// in the process's stdin. One write at a time. `None` for a stdin closed, or never open, and
    /// for a write its process went away from before an answer; a write not `whole` closes it.
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
            stdin.waiting.insert(seq, told);
            stdin.open = !eof;
            written
        };
        Ok(written.await.ok())
    }

    /// The guest has answered the write `seq` to its stdin. One not `whole` leaves its stdin
    /// closed: its reader is gone.
    pub(super) fn written(&self, seq: u64, written: Written) {
        let mut stdin = self.stdin.lock().unwrap();
        if !written.whole {
            stdin.open = false;
        }
        if let Some(told) = stdin.waiting.remove(&seq) {
            let _ = told.send(written);
        }
    }
}
