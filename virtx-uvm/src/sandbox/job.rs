use std::sync::{Arc, Mutex};
use std::time::Instant;

use virtx_core::{Log, SandboxJob, SandboxJobEnd, SandboxJobEndReason, SandboxJobEntry};

use super::sandbox::{LOG_LIMIT, SIGKILL};

/// A process in a sandbox, as it is answered, and its log.
pub struct Job {
    job: Mutex<SandboxJob>,
    log: Arc<Log<SandboxJobEntry>>,
    /// Why the server kills it, once it has set out to: what its end says if it is killed.
    killing: Mutex<Option<SandboxJobEndReason>>,
    /// When it ended, once it has: of the ended jobs a sandbox keeps, the one that ended first
    /// goes first.
    ended_at: Mutex<Option<Instant>>,
}

impl Job {
    pub(super) fn new(job: SandboxJob) -> Arc<Job> {
        Arc::new(Job {
            job: Mutex::new(job),
            log: Log::new(LOG_LIMIT, SandboxJobEntry::Truncated {}),
            killing: Mutex::new(None),
            ended_at: Mutex::new(None),
        })
    }

    /// `job`, as a sandbox kept past its process had it, known again: ended as it ended, or,
    /// one that ran then, as killed for its server's restart. Its log is its end alone.
    pub(super) fn kept(mut job: SandboxJob) -> Arc<Job> {
        if job.running {
            (job.running, job.code) = (false, Some(128 + SIGKILL));
            job.reason = Some(SandboxJobEndReason::Restarted);
        }
        let end = SandboxJobEnd {
            code: job.code.unwrap_or(128 + SIGKILL),
            reason: job.reason,
        };
        let kept = Job::new(job);
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

    fn end_as(&self, end: SandboxJobEnd) {
        let (code, reason) = (end.code, end.reason);
        self.log.end(SandboxJobEntry::End(end), || {
            let mut job = self.job.lock().unwrap();
            (job.running, job.code, job.reason) = (false, Some(code), reason);
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
}
