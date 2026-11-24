//! Every flow to the internet this process's sandboxes' machines opened, or were refused, as
//! each machine's process reported it (see `machine/net/policy.rs`): the one record of which
//! sandbox opened which connection, since off this host every machine's flows are the host's.
//! Kept and asked for as [`runs`](super::runs) are: numbered in order, woken as each is told
//! ([`subscribe`]), then asking for those after the last one had ([`since`]), from the first
//! [`subscribe`] on and for this process alone.
//!
//! What each sandbox's machine may open is the embedder's to say ([`set_limits`]), asked as
//! each machine boots.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex, RwLock};

use tokio::sync::watch;

pub use crate::machine::EgressLimits;
pub use crate::machine::wire::{Outbound, Protocol, Refusal, Refused};

/// How many are kept to be asked for: past it, the oldest go first.
const KEPT: usize = 100_000;

/// One flow a sandbox's machine opened, or flows it was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Flow {
    /// Its number: one more than the one before it, of any sandbox in this process.
    pub seq: u64,
    /// The sandbox's id, and the root it was made under.
    pub sandbox: String,
    pub root: PathBuf,
    pub outbound: Outbound,
}

/// What [`since`] answers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Since {
    /// What was told after the number asked after, oldest first, at most as many as asked.
    pub flows: Vec<Flow>,
    /// The number to ask after next: the last of `flows`, or the last there is where `flows`
    /// is empty.
    pub last: u64,
    /// Whether nothing between the number asked after and `flows` is gone.
    pub all: bool,
}

struct Ledger {
    /// The number the next takes.
    next: u64,
    kept: VecDeque<Flow>,
}

static LEDGER: LazyLock<Mutex<Ledger>> = LazyLock::new(|| {
    Mutex::new(Ledger {
        next: 1,
        kept: VecDeque::new(),
    })
});

/// Whether anything is kept: from the first [`subscribe`] on.
static KEEPING: AtomicBool = AtomicBool::new(false);

/// The number of the last, as each is told: what a subscriber is woken by.
static LAST: LazyLock<watch::Sender<u64>> = LazyLock::new(|| watch::Sender::new(0));

/// What a sandbox's machine under a root may open, as the embedder says.
type Limits = Arc<dyn Fn(&Path) -> Option<EgressLimits> + Send + Sync>;

static LIMITS: RwLock<Option<Limits>> = RwLock::new(None);

/// What each sandbox's machine in this process may open, from its next boot on: `limits` of
/// the root it was made under, none being no limit. Said again, the last is what is asked.
pub fn set_limits(limits: impl Fn(&Path) -> Option<EgressLimits> + Send + Sync + 'static) {
    *LIMITS.write().unwrap() = Some(Arc::new(limits));
}

/// What a machine of a sandbox under `root` may open.
pub(super) fn limits(root: &Path) -> Option<EgressLimits> {
    let limits = LIMITS.read().unwrap().clone();
    limits.and_then(|limits| limits(root))
}

/// Sandbox `sandbox`'s machine, under `root`, reported `outbound`.
pub(super) fn told(sandbox: &str, root: &Path, outbound: Outbound) {
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
        ledger.kept.push_back(Flow {
            seq,
            sandbox: sandbox.to_owned(),
            root: root.to_owned(),
            outbound,
        });
        seq
    };
    LAST.send_replace(seq);
}

/// What was told after `seq`, oldest first, at most `most` of it: the rest is asked for after
/// the [`last`](Since::last) of these.
pub fn since(seq: u64, most: usize) -> Since {
    let ledger = LEDGER.lock().unwrap();
    let newest = ledger.next - 1;
    let first_kept = ledger.kept.front().map_or(ledger.next, |flow| flow.seq);
    let from = seq.saturating_add(1).max(first_kept);
    let skip = usize::try_from(from - first_kept).unwrap_or(usize::MAX);
    let flows: Vec<Flow> = ledger.kept.iter().skip(skip).take(most).cloned().collect();
    Since {
        last: flows.last().map_or(newest, |flow| flow.seq),
        all: seq >= newest || from == seq + 1,
        flows,
    }
}

/// Woken as each is told, with the number of the last. The first keeps each from then on.
pub fn subscribe() -> watch::Receiver<u64> {
    KEEPING.store(true, Ordering::Relaxed);
    LAST.subscribe()
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::time::SystemTime;

    use super::*;

    fn outbound(to: &str) -> Outbound {
        Outbound {
            at: SystemTime::UNIX_EPOCH,
            protocol: Protocol::Tcp,
            to: to.parse::<SocketAddr>().unwrap(),
            refused: None,
        }
    }

    /// One test, since the ledger is the process's and a test beside it would move it.
    #[test]
    fn what_was_told_is_asked_for_after_the_last_known_in_order_with_its_sandbox() {
        let root = Path::new("/home/a");
        // Nothing is kept until someone subscribes.
        told("sbx_none", root, outbound("1.1.1.1:443"));
        assert_eq!(since(0, usize::MAX).flows, Vec::new());

        let mut woken = subscribe();
        let before = since(0, usize::MAX).last;
        told("sbx_a", root, outbound("1.1.1.1:443"));
        told("sbx_b", root, outbound("8.8.8.8:53"));
        assert_eq!(*woken.borrow_and_update(), before + 2);
        let told_since = since(before, usize::MAX);
        let ours: Vec<_> = (told_since.flows.iter())
            .map(|f| (f.seq, f.sandbox.as_str(), f.outbound.to.to_string()))
            .collect();
        assert_eq!(
            ours,
            [
                (before + 1, "sbx_a", "1.1.1.1:443".to_owned()),
                (before + 2, "sbx_b", "8.8.8.8:53".to_owned()),
            ]
        );
        assert!(told_since.all);
        let one = since(before, 1);
        assert_eq!((one.flows.len(), one.last), (1, before + 1));

        // Past what is kept, the oldest go, and asking after one gone is told it is not all.
        for _ in 0..KEPT {
            told("sbx_c", root, outbound("1.1.1.1:443"));
        }
        let told_since = since(before, usize::MAX);
        assert!(!told_since.all);
        assert_eq!(told_since.flows.len(), KEPT);
    }

    /// The embedder's limits are asked by root, the last it set being what answers.
    #[test]
    fn limits_are_the_embedder_s_by_root() {
        let limits = EgressLimits {
            connects_per_second: 5,
            destinations_per_minute: 10,
        };
        set_limits(move |root| (root == Path::new("/home/limited")).then_some(limits));
        assert_eq!(super::limits(Path::new("/home/limited")), Some(limits));
        assert_eq!(super::limits(Path::new("/home/other")), None);
        set_limits(|_| None);
        assert_eq!(super::limits(Path::new("/home/limited")), None);
    }
}
