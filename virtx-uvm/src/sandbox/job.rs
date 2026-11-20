use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::{oneshot, watch};
use virtx_core::{Log, SandboxJob, SandboxJobEnd, SandboxJobEndReason, SandboxJobEntry};

use super::sandbox::{LOG_LIMIT, SIGKILL};
use crate::machine::wire::Taken;

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
    /// killed it, if it did, or for its memory running out where the guest says `oom`.
    pub(super) fn end(&self, code: i32, oom: bool) {
        let killed = code == 128 + SIGKILL;
        let reason = killed.then(|| *self.killing.lock().unwrap()).flatten();
        let reason = reason.or((killed && oom).then_some(SandboxJobEndReason::Oom));
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
    /// in the process's stdin or never will be. One write at a time. `None` for a stdin closed,
    /// or never open, and for a write its process went away from before an answer.
    pub(super) async fn write_stdin(
        &self,
        eof: bool,
        send: impl FnOnce(u64) -> Result<(), super::Error>,
    ) -> Result<Option<Written>, super::Error> {
        let _turn = self.writing.lock().await;
        // Registered before the send, so its answer finds it. Sent unlocked: the send blocks
        // until the guest reads it, and the reader of the guest's answers takes this lock.
        let (seq, written) = {
            let mut stdin = self.stdin.lock().unwrap();
            if !stdin.open {
                return Ok(None);
            }
            let seq = stdin.next;
            stdin.next += 1;
            let (told, written) = oneshot::channel();
            stdin.waiting.insert(seq, (eof, told));
            stdin.open = !eof;
            (seq, written)
        };
        if let Err(e) = send(seq) {
            // Unsent: as if never asked, unless the job ended or its stdin went meanwhile.
            // `next` stays: part of it may have reached the guest.
            let mut stdin = self.stdin.lock().unwrap();
            if stdin.waiting.remove(&seq).is_some() && eof && !stdin.gone {
                stdin.open = true;
            }
            return Err(e);
        }
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

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::Duration;

    use virtx_core::SandboxJobKind;

    use super::*;
    use crate::sandbox::Error;

    fn job() -> Arc<Job> {
        let job = SandboxJob {
            id: "j".into(),
            kind: SandboxJobKind::Exec,
            pid: 7,
            cmd: vec!["cat".into()],
            running: true,
            code: None,
            reason: None,
        };
        Job::new(job, true)
    }

    #[tokio::test]
    async fn an_answer_read_while_its_write_is_sent_finds_the_write_waiting() {
        let job = job();
        let answering = Arc::clone(&job);
        let send = move |seq| {
            // Answered on another thread before the send returns, as a large write can be.
            let (said, heard) = mpsc::channel();
            std::thread::spawn(move || {
                let written = Written {
                    bytes: 2,
                    taken: Taken::Whole,
                };
                answering.written(seq, written);
                let _ = said.send(());
            });
            heard
                .recv_timeout(Duration::from_secs(5))
                .map_err(|_| Error::JobEnded("the answer waited on the send"))
        };
        let written = job.write_stdin(false, send).await.unwrap();
        assert_eq!(written.map(|w| w.taken), Some(Taken::Whole));
    }

    #[tokio::test]
    async fn a_write_not_sent_reopens_its_stdin_only_if_nothing_closed_it_meanwhile() {
        let cases = [
            ("nothing", (|_| {}) as fn(&Job), true),
            ("its job ends", |job| job.end(0, false), false),
            (
                "its stdin goes",
                |job| {
                    let mut stdin = job.stdin.lock().unwrap();
                    (stdin.open, stdin.gone) = (false, true);
                },
                false,
            ),
        ];
        for (meanwhile, happens, open) in cases {
            let job = job();
            let during = Arc::clone(&job);
            let send = move |_| {
                happens(&during);
                Err(Error::JobEnded("not sent"))
            };
            assert!(job.write_stdin(true, send).await.is_err(), "{meanwhile}");
            let stdin = job.stdin.lock().unwrap();
            assert_eq!(
                (stdin.open, stdin.waiting.is_empty()),
                (open, true),
                "{meanwhile}"
            );
        }
    }
}
