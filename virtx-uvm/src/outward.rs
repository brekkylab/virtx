//! What an embedder decides of each flow a sandbox's machine opens to the internet, and what it
//! hears of them.
//!
//! Every flow a guest on a [`Public`](crate::Network::Public) network opens to an address the
//! internet routes -- a `connect`, or a datagram socket's destination -- is put to the
//! embedder's [`Outward`] in the machine's process, which is the embedder's own binary as
//! [`Machine::start`](crate::Machine::start) spawns it. It says whether the flow goes, by what
//! the server gave that machine as it booted ([`set_given`]), and may [`tell`] the server
//! anything, which reaches the server's [`set_told`] with the sandbox it came from. With no
//! [`Outward`], every such flow goes and nothing is told.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, OnceLock, RwLock};

use serde_json::Value;

/// What decides each flow to the internet, in a machine's process.
pub trait Outward: Send + Sync {
    /// Whether the guest's flow to `to` goes; refused, a `connect` is `ECONNREFUSED` in the
    /// guest and a datagram is dropped. `given` is what [`set_given`] answered for the
    /// machine's sandbox, `Null` for nothing. Asked once for each `connect`, and for each
    /// destination a datagram socket sends to, the two alike.
    fn outward(&self, given: &Value, to: SocketAddr) -> bool;
}

static OUTWARD: OnceLock<Arc<dyn Outward>> = OnceLock::new();

/// Flows to the internet are decided by `outward` in this process, from now on: said once, in
/// the embedder's binary before it becomes a machine. `false`, and nothing changed, where it
/// was said already.
pub fn set_outward(outward: Arc<dyn Outward>) -> bool {
    OUTWARD.set(outward).is_ok()
}

/// What decides flows to the internet in this process, if anything does.
pub(crate) fn outward() -> Option<Arc<dyn Outward>> {
    OUTWARD.get().cloned()
}

/// Tell the server that started this machine `message`, from its process: what reaches the
/// server's [`set_told`]. Nothing outside a machine's process.
pub fn tell(message: Value) {
    crate::machine::tell_host(message);
}

/// What each sandbox's machine's [`Outward`] is given by root.
type Given = Arc<dyn Fn(&Path) -> Option<Value> + Send + Sync>;
/// Where what machines [`tell`] goes, with the sandbox's id and root.
type Told = Arc<dyn Fn(&str, &Path, Value) + Send + Sync>;

static GIVEN: RwLock<Option<Given>> = RwLock::new(None);
static TOLD: RwLock<Option<Told>> = RwLock::new(None);

/// What the [`Outward`] of each sandbox's machine in this server is given, by the root the
/// sandbox was made under: asked as each machine boots. Said again, the last is what is asked.
pub fn set_given(given: impl Fn(&Path) -> Option<Value> + Send + Sync + 'static) {
    *GIVEN.write().unwrap() = Some(Arc::new(given));
}

/// Where what this server's sandboxes' machines [`tell`] goes: `told` with the sandbox's id
/// and root, in the order each machine told it, from a thread of the sandbox's own. Said
/// again, the last is where it goes.
pub fn set_told(told: impl Fn(&str, &Path, Value) + Send + Sync + 'static) {
    *TOLD.write().unwrap() = Some(Arc::new(told));
}

/// What a machine of a sandbox under `root` is given.
pub(crate) fn given(root: &Path) -> Option<Value> {
    let given = GIVEN.read().unwrap().clone();
    given.and_then(|given| given(root))
}

/// Sandbox `sandbox`, under `root`, told `message`.
pub(crate) fn told(sandbox: &str, root: &Path, message: Value) {
    let told = TOLD.read().unwrap().clone();
    if let Some(told) = told {
        told(sandbox, root, message);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One test, since what is set is the process's.
    #[test]
    fn what_is_given_and_told_is_the_embedder_s_last_said() {
        assert_eq!(given(Path::new("/home/a")), None);
        set_given(|root| (root == Path::new("/home/a")).then(|| serde_json::json!({"n": 1})));
        assert_eq!(
            given(Path::new("/home/a")),
            Some(serde_json::json!({"n": 1}))
        );
        assert_eq!(given(Path::new("/home/b")), None);

        let heard = Arc::new(std::sync::Mutex::new(Vec::new()));
        let kept = Arc::clone(&heard);
        set_told(move |sandbox, root, message| {
            kept.lock()
                .unwrap()
                .push((sandbox.to_owned(), root.to_owned(), message))
        });
        told("sbx", Path::new("/home/a"), serde_json::json!("hi"));
        assert_eq!(
            *heard.lock().unwrap(),
            [("sbx".into(), "/home/a".into(), serde_json::json!("hi"))]
        );
    }
}
