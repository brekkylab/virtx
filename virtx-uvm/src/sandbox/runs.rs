//! Every machine of this process's sandboxes, as it came up and as it went down -- parked or
//! ended -- each told with when: what an embedder meters machine time by, exactly, however
//! briefly a machine was up. Each is numbered in the order it happened, one after another, so
//! that whoever is told of them -- as each happens ([`subscribe`]), or by asking for those
//! after the last it has ([`since`]) -- knows when it missed one.

use std::collections::VecDeque;
use std::sync::{LazyLock, Mutex};
use std::time::SystemTime;

use tokio::sync::watch;

/// How many are kept to be asked for: past it, the oldest go first.
const KEPT: usize = 100_000;

/// What a sandbox's machine did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ran {
    /// Came up at `at`.
    Up { at: SystemTime },
    /// Went down -- parked, or ended -- having been up from `started` to `ended`.
    Down {
        started: SystemTime,
        ended: SystemTime,
    },
}

/// One thing a sandbox's machine did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MachineRun {
    /// Its number: one more than the one before it, of any sandbox here.
    pub seq: u64,
    pub sandbox: String,
    pub ran: Ran,
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

/// The number of the last, as each is told: what a subscriber is woken by.
static LAST: LazyLock<watch::Sender<u64>> = LazyLock::new(|| watch::Sender::new(0));

fn told(sandbox: &str, ran: Ran) {
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
            ran,
        });
        seq
    };
    LAST.send_replace(seq);
}

/// Sandbox `sandbox`'s machine up now.
pub(super) fn up(sandbox: &str, at: SystemTime) {
    told(sandbox, Ran::Up { at });
}

/// Sandbox `sandbox`'s machine, up since `started`, down now.
pub(super) fn down(sandbox: &str, started: SystemTime) {
    told(
        sandbox,
        Ran::Down {
            started,
            ended: SystemTime::now(),
        },
    );
}

/// What happened after `seq`, oldest first; the number of the last, which is asked after next;
/// and whether that is all of it -- false where some of it is kept no longer.
pub fn since(seq: u64) -> (Vec<MachineRun>, u64, bool) {
    let ledger = LEDGER.lock().unwrap();
    let last = ledger.next - 1;
    let runs: Vec<MachineRun> = ledger
        .kept
        .iter()
        .filter(|run| run.seq > seq)
        .cloned()
        .collect();
    let all = seq >= last || runs.first().is_some_and(|first| first.seq == seq + 1);
    (runs, last, all)
}

/// Woken as each happens, with the number of the last: what an embedder tells others by as it
/// happens, asking [`since`] for what it has not told.
pub fn subscribe() -> watch::Receiver<u64> {
    LAST.subscribe()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_ran_is_asked_for_after_the_last_known_in_the_order_it_happened() {
        let mut woken = subscribe();
        let (_, before, _) = since(0);
        up("sbx_a", SystemTime::UNIX_EPOCH);
        down("sbx_a", SystemTime::UNIX_EPOCH);
        assert!(woken.has_changed().unwrap());
        assert!(*woken.borrow_and_update() >= before + 2);
        let (runs, last, all) = since(before);
        let ours: Vec<_> = runs
            .iter()
            .filter(|r| r.sandbox == "sbx_a")
            .map(|r| matches!(r.ran, Ran::Up { .. }))
            .collect();
        assert_eq!(ours, [true, false]);
        assert!(all);
        assert!(since(last).0.iter().all(|r| r.seq > last));
    }
}
