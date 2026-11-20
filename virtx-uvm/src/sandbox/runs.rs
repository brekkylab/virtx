//! Every machine of this process's sandboxes, as it came up and as it went down -- parked,
//! closed or ended on its own -- each told with when, and how long it was up by a clock that only
//! moves forward: what an embedder meters machine time by, exactly, however briefly a machine was
//! up and however the wall clock was set meanwhile. Each is numbered in the order it happened, one
//! after another, so whoever is told of them -- woken as each happens ([`subscribe`]), then
//! asking for those after the last it has ([`since`]) -- knows when it missed one.
//!
//! The ledger is the process's, every server's in it together, which an embedder with more than
//! one tells apart by [`MachineRun::root`]. It is kept in memory from the first [`subscribe`]
//! on, so a process that never asks keeps nothing, and goes with the process: a number is of
//! one process's ledger ([`Since::ledger`]), and means nothing to the next.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime};

use tokio::sync::watch;

/// How many are kept to be asked for: past it, the oldest go first.
const KEPT: usize = 100_000;

/// What a sandbox's machine did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ran {
    /// Came up at `at`.
    Up { at: SystemTime },
    /// Went down, having been up from `started` to `ended` by the wall clock, and for `took` by
    /// a clock no one sets: what to meter by, where the two differ.
    Down {
        started: SystemTime,
        ended: SystemTime,
        took: Duration,
    },
}

/// One thing a sandbox's machine did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MachineRun {
    /// Its number: one more than the one before it, of any sandbox in this process.
    pub seq: u64,
    /// The sandbox's id, and the root it was made under, which tells apart two sandboxes of
    /// one id under different roots, and two servers' in one process.
    pub sandbox: String,
    pub root: PathBuf,
    pub ran: Ran,
}

/// What [`since`] answers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Since {
    /// What happened after the number asked after, oldest first, at most as many as asked.
    pub runs: Vec<MachineRun>,
    /// The number to ask after next: the last of `runs`, or the last there is where `runs` is
    /// empty.
    pub last: u64,
    /// Whether nothing between the number asked after and `runs` is gone: false where some of it
    /// is kept no longer.
    pub all: bool,
    /// This process's ledger, made as it starts: a number kept from a ledger of another id is no
    /// number of this one, and whatever it had not been told of is gone.
    pub ledger: String,
}

struct Ledger {
    /// The number the next takes.
    next: u64,
    kept: VecDeque<MachineRun>,
}

static LEDGER: LazyLock<Mutex<Ledger>> = LazyLock::new(|| {
    Mutex::new(Ledger {
        next: 1,
        kept: VecDeque::new(),
    })
});

/// This process's ledger's id.
static LEDGER_ID: LazyLock<String> = LazyLock::new(|| uuid::Uuid::new_v4().simple().to_string());

/// Whether anything is kept: from the first [`subscribe`] on.
static KEEPING: AtomicBool = AtomicBool::new(false);

/// The number of the last, as each is told: what a subscriber is woken by.
static LAST: LazyLock<watch::Sender<u64>> = LazyLock::new(|| watch::Sender::new(0));

fn told(sandbox: &str, root: &Path, ran: Ran) {
    if !KEEPING.load(Ordering::Relaxed) {
        return;
    }
    let seq = {
        let mut ledger = LEDGER.lock().unwrap();
        let seq = ledger.next;
        ledger.next += 1;
        if ledger.kept.len() == KEPT {
            ledger.kept.pop_front();
        }
        ledger.kept.push_back(MachineRun {
            seq,
            sandbox: sandbox.to_owned(),
            root: root.to_owned(),
            ran,
        });
        seq
    };
    LAST.send_replace(seq);
}

/// Sandbox `sandbox`'s machine, under `root`, up since `at`.
pub(super) fn up(sandbox: &str, root: &Path, at: SystemTime) {
    told(sandbox, root, Ran::Up { at });
}

/// Sandbox `sandbox`'s machine, under `root`, up since `started` -- `since` by the clock no one
/// sets -- down now.
pub(super) fn down(sandbox: &str, root: &Path, started: SystemTime, since: Instant) {
    let ended = SystemTime::now();
    let took = since.elapsed();
    told(
        sandbox,
        root,
        Ran::Down {
            started,
            ended,
            took,
        },
    );
}

/// What happened after `seq`, oldest first, at most `most` of it: the rest is asked for after
/// the [`last`](Since::last) of these.
pub fn since(seq: u64, most: usize) -> Since {
    let ledger = LEDGER.lock().unwrap();
    let newest = ledger.next - 1;
    let first_kept = ledger.kept.front().map_or(ledger.next, |run| run.seq);
    let from = seq.saturating_add(1).max(first_kept);
    let skip = usize::try_from(from - first_kept).unwrap_or(usize::MAX);
    let runs: Vec<MachineRun> = ledger.kept.iter().skip(skip).take(most).cloned().collect();
    Since {
        last: runs.last().map_or(newest, |run| run.seq),
        all: seq >= newest || from == seq + 1,
        runs,
        ledger: LEDGER_ID.clone(),
    }
}

/// Woken as each happens, with the number of the last: what an embedder tells others by as it
/// happens, asking [`since`] for what it has not told. The first keeps each from then on.
pub fn subscribe() -> watch::Receiver<u64> {
    KEEPING.store(true, Ordering::Relaxed);
    LAST.subscribe()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One test, since the ledger is the process's and a test that runs beside it would move it.
    #[test]
    fn what_ran_is_asked_for_after_the_last_known_in_order_and_what_is_gone_said_so() {
        let root = Path::new("/home/a");
        // Nothing is kept until someone subscribes.
        up("sbx_none", root, SystemTime::UNIX_EPOCH);
        assert_eq!(since(0, usize::MAX).runs, Vec::new());

        let mut woken = subscribe();
        let before = since(0, usize::MAX).last;
        up("sbx_a", root, SystemTime::UNIX_EPOCH);
        down("sbx_a", root, SystemTime::UNIX_EPOCH, Instant::now());
        assert!(woken.has_changed().unwrap());
        assert_eq!(*woken.borrow_and_update(), before + 2);
        let told = since(before, usize::MAX);
        let ours: Vec<_> = (told.runs.iter())
            .map(|r| {
                (
                    r.seq,
                    r.sandbox.as_str(),
                    r.root.as_path(),
                    matches!(r.ran, Ran::Up { .. }),
                )
            })
            .collect();
        assert_eq!(
            ours,
            [
                (before + 1, "sbx_a", root, true),
                (before + 2, "sbx_a", root, false)
            ]
        );
        assert_eq!(told.last, before + 2);
        assert!(told.all);
        let after = since(told.last, usize::MAX);
        assert_eq!(
            (after.runs, after.last, after.all),
            (Vec::new(), told.last, true)
        );
        assert_eq!(after.ledger, told.ledger, "one ledger in one process");

        // At most as many as asked, the rest asked for after the last of them.
        let one = since(before, 1);
        assert_eq!((one.runs.len(), one.last, one.all), (1, before + 1, true));
        let rest = since(one.last, 1);
        assert_eq!(
            (rest.runs.len(), rest.last, rest.all),
            (1, before + 2, true)
        );

        // Past what is kept, the oldest go, and asking after one gone is told it is not all.
        for _ in 0..KEPT {
            up("sbx_b", root, SystemTime::UNIX_EPOCH);
        }
        let told = since(before, usize::MAX);
        assert!(!told.all, "the first after it is kept no longer");
        assert_eq!(told.runs.len(), KEPT);
        assert_eq!(told.runs.last().unwrap().seq, told.last);
        assert!(since(told.last - 1, usize::MAX).all);
    }
}
