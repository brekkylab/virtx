use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use tokio::sync::{oneshot, watch};
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
    /// Its stdin, and the writes sent to it not yet written.
    stdin: Mutex<Stdin>,
    /// Held while a write to its stdin is under way, so they go one at a time.
    writing: tokio::sync::Mutex<()>,
}

/// A job's stdin: whether it takes writes still, and who waits for each sent to be written, in
/// the order sent.
#[derive(Default)]
struct Stdin {
    open: bool,
    waiting: VecDeque<oneshot::Sender<bool>>,
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
                waiting: VecDeque::new(),
            }),
            writing: tokio::sync::Mutex::new(()),
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

    pub(super) fn id(&self) -> String {
        self.job.lock().unwrap().id.clone()
    }

    /// Send what `send` sends to its stdin, if it is open, closing it if `eof`: done once the
    /// guest says it is [written](Job::written), so a process that does not read holds back
    /// whoever writes to it and nothing else. One write at a time. `false` for a stdin closed,
    /// or never open, and for a write never written: its process gone first.
    pub(super) async fn write_stdin(
        &self,
        eof: bool,
        send: impl FnOnce() -> Result<(), super::Error>,
    ) -> Result<bool, super::Error> {
        let _turn = self.writing.lock().await;
        let written = {
            // Sent and waited for under one lock, so its answer finds it waiting.
            let mut stdin = self.stdin.lock().unwrap();
            if !stdin.open {
                return Ok(false);
            }
            send()?;
            let (told, written) = oneshot::channel();
            stdin.waiting.push_back(told);
            stdin.open = !eof;
            written
        };
        Ok(written.await.unwrap_or(false))
    }

    /// The guest has written the oldest write to its stdin not yet answered, if `written`, or
    /// never will.
    pub(super) fn written(&self, written: bool) {
        if let Some(told) = self.stdin.lock().unwrap().waiting.pop_front() {
            let _ = told.send(written);
        }
    }
}
