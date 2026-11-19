use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use tokio::sync::oneshot;
use virtx_core::{Log, SandboxJob, SandboxJobEnd, SandboxJobEntry};

use super::sandbox::LOG_LIMIT;

/// A process in a sandbox, as it is answered, and its log.
pub struct Job {
    job: Mutex<SandboxJob>,
    log: Arc<Log<SandboxJobEntry>>,
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

    pub(super) fn write(&self, entry: SandboxJobEntry) {
        self.log.push(entry);
    }

    /// End it with `code`, if it runs still. What its stdin was sent and has not taken never
    /// will be: whoever waits on it is told so.
    pub(super) fn end(&self, code: i32) {
        *self.stdin.lock().unwrap() = Stdin::default();
        self.log
            .end(SandboxJobEntry::End(SandboxJobEnd { code }), || {
                let mut job = self.job.lock().unwrap();
                (job.running, job.code) = (false, Some(code));
            });
    }

    pub(super) fn remove(&self) {
        self.log.remove();
    }

    /// Its pid, and whether it runs still.
    pub(super) fn pid(&self) -> (u32, bool) {
        let job = self.job.lock().unwrap();
        (job.pid, job.running)
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
