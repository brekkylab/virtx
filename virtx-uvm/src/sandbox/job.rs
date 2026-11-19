use std::sync::{Arc, Mutex};
use std::time::Instant;

use virtx_core::{Log, SandboxJob, SandboxJobEnd, SandboxJobEndReason, SandboxJobEntry};

use super::sandbox::LOG_LIMIT;

/// A process in a sandbox, as it is answered, and its log.
pub struct Job {
    job: Mutex<SandboxJob>,
    log: Arc<Log<SandboxJobEntry>>,
    /// When it ended, once it has: of the ended jobs a sandbox keeps, the one that ended first
    /// goes first.
    ended_at: Mutex<Option<Instant>>,
}

impl Job {
    pub(super) fn new(job: SandboxJob) -> Arc<Job> {
        Arc::new(Job {
            job: Mutex::new(job),
            log: Log::new(LOG_LIMIT, SandboxJobEntry::Truncated {}),
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

    /// End it with `code`, if it runs still.
    pub(super) fn end(&self, code: i32) {
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
}
