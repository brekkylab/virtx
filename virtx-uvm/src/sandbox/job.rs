use std::sync::{Arc, Mutex};

use tokio::sync::watch;
use virtx_core::{Log, SandboxJob, SandboxJobEnd, SandboxJobEndReason, SandboxJobEntry};

use super::sandbox::LOG_LIMIT;

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
}

impl Job {
    pub(super) fn new(job: SandboxJob) -> Arc<Job> {
        Arc::new(Job {
            job: Mutex::new(job),
            log: Log::new(LOG_LIMIT, SandboxJobEntry::Truncated {}),
            code: watch::channel(None).0,
            ended_at: Mutex::new(None),
            killing: Mutex::new(None),
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
        let killed = code == 128 + super::sandbox::SIGKILL;
        let reason = killed.then(|| *self.killing.lock().unwrap()).flatten();
        self.end_as(SandboxJobEnd { code, reason });
    }

    /// End it now, if it runs still, as killed for `reason`, or for the reason it is being
    /// killed for already: whatever its process does after is not its job's.
    pub(super) fn cut(&self, reason: SandboxJobEndReason) {
        let reason = self.killing.lock().unwrap().unwrap_or(reason);
        self.end_as(SandboxJobEnd {
            code: 128 + super::sandbox::SIGKILL,
            reason: Some(reason),
        });
    }

    fn end_as(&self, end: SandboxJobEnd) {
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

    pub(super) fn id(&self) -> String {
        self.job.lock().unwrap().id.clone()
    }
}
