use std::sync::{Arc, Mutex};

use tokio::sync::watch;
use virtx_core::{Log, SandboxJob, SandboxJobEnd, SandboxJobEntry};

use super::sandbox::LOG_LIMIT;

/// A process in a sandbox, as it is answered, and its log.
pub struct Job {
    job: Mutex<SandboxJob>,
    log: Arc<Log<SandboxJobEntry>>,
    /// Its code, once it has ended.
    code: watch::Sender<Option<i32>>,
    /// When it ended, once it has: the oldest ended go first ([`super::sandbox::KEEP_ENDED`]).
    ended_at: Mutex<Option<std::time::Instant>>,
}

impl Job {
    pub(super) fn new(job: SandboxJob) -> Arc<Job> {
        Arc::new(Job {
            job: Mutex::new(job),
            log: Log::new(LOG_LIMIT, SandboxJobEntry::Truncated {}),
            code: watch::channel(None).0,
            ended_at: Mutex::new(None),
        })
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

    /// End it with `code`, if it runs still.
    pub(super) fn end(&self, code: i32) {
        let ended = self
            .log
            .end(SandboxJobEntry::End(SandboxJobEnd { code }), || {
                let mut job = self.job.lock().unwrap();
                (job.running, job.code) = (false, Some(code));
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

    pub(super) fn id(&self) -> String {
        self.job.lock().unwrap().id.clone()
    }
}
