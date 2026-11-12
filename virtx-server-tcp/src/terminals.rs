//! Terminals kept across connections: a shell on a terminal in a sandbox ([`Terminal`]), with
//! the last [`BACKLOG`] of what it wrote, under an id of its own -- a session. A connection
//! attaches to one, and detaches as it closes, the shell left running; one that names it again
//! is answered what it wrote, and carries on where it was. A later connection takes it over
//! from an earlier one.
//!
//! A session ends with its shell: as it exits, as it is ended ([`Session::end`]), as its
//! sandbox stops, or as a sandbox has more than [`MOST`] and it is the oldest left detached.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Instant;

use tokio::sync::mpsc;

use crate::endpoints::{Refused, Terminal};

/// How much of what a session's shell wrote is kept, to answer a connection attaching to it.
pub const BACKLOG: usize = 256 << 10;
/// How many sessions a sandbox keeps at most.
pub const MOST: usize = 8;

/// What a connection attached to a session is told.
pub enum Shown {
    /// What the shell wrote.
    Data(String),
    /// The shell ended, with its code.
    Exit(i32),
}

/// Every session of a home's sandboxes, by id.
#[derive(Default)]
pub struct Sessions(Mutex<HashMap<String, Arc<Session>>>);

pub struct Session {
    pub id: String,
    pub sandbox: String,
    /// The shell's terminal, until it ends: dropped, the shell is killed.
    terminal: Mutex<Option<Terminal>>,
    backlog: Mutex<(VecDeque<String>, usize)>,
    /// The connection attached, if one is: which, and where what is shown goes.
    attached: Mutex<Option<(u64, mpsc::UnboundedSender<Shown>)>>,
    attaches: AtomicU64,
    opened: Instant,
}

impl Sessions {
    /// Session `id` of sandbox `sandbox`, if it is still.
    pub fn get(&self, id: &str, sandbox: &str) -> Option<Arc<Session>> {
        self.0
            .lock()
            .unwrap()
            .get(id)
            .filter(|session| session.sandbox == sandbox)
            .cloned()
    }

    /// A session of `terminal`, a shell on a terminal in sandbox `sandbox`, under an id of its
    /// own: kept until it ends, what it writes kept and shown to whoever is attached. The
    /// oldest detached of the sandbox's ended, past [`MOST`].
    pub fn start(self: &Arc<Self>, sandbox: &str, mut terminal: Terminal) -> Arc<Session> {
        let (_, closed) = mpsc::unbounded_channel();
        let output = std::mem::replace(&mut terminal.output, closed);
        let session = Arc::new(Session {
            id: format!(
                "{}{}",
                uuid::Uuid::new_v4().simple(),
                uuid::Uuid::new_v4().simple()
            ),
            sandbox: sandbox.to_owned(),
            terminal: Mutex::new(Some(terminal)),
            backlog: Mutex::default(),
            attached: Mutex::default(),
            attaches: AtomicU64::new(0),
            opened: Instant::now(),
        });
        let evicted = {
            let mut sessions = self.0.lock().unwrap();
            sessions.insert(session.id.clone(), Arc::clone(&session));
            let mut its: Vec<_> = sessions
                .values()
                .filter(|s| s.sandbox == sandbox && s.attached.lock().unwrap().is_none())
                .cloned()
                .collect();
            let all = sessions.values().filter(|s| s.sandbox == sandbox).count();
            its.sort_by_key(|s| s.opened);
            its.into_iter()
                .take(all.saturating_sub(MOST))
                .collect::<Vec<_>>()
        };
        for old in evicted {
            old.end();
        }
        tokio::spawn(pump(Arc::downgrade(self), Arc::clone(&session), output));
        session
    }

    fn remove(&self, id: &str) {
        self.0.lock().unwrap().remove(id);
    }
}

/// What the shell of `session` writes, kept and shown, until it ends: then its end shown, and
/// the session gone.
async fn pump(
    sessions: Weak<Sessions>,
    session: Arc<Session>,
    mut output: mpsc::UnboundedReceiver<String>,
) {
    while let Some(data) = output.recv().await {
        {
            let mut backlog = session.backlog.lock().unwrap();
            backlog.1 += data.len();
            backlog.0.push_back(data.clone());
            while backlog.1 > BACKLOG && backlog.0.len() > 1 {
                let gone = backlog.0.pop_front().unwrap_or_default();
                backlog.1 -= gone.len();
            }
        }
        if let Some((_, to)) = &*session.attached.lock().unwrap() {
            let _ = to.send(Shown::Data(data));
        }
    }
    let terminal = session.terminal.lock().unwrap().take();
    let code = match &terminal {
        Some(terminal) => terminal.ended().await,
        None => 128 + 9,
    };
    if let Some(sessions) = sessions.upgrade() {
        sessions.remove(&session.id);
    }
    if let Some((_, to)) = session.attached.lock().unwrap().take() {
        let _ = to.send(Shown::Exit(code));
    }
}

impl Session {
    /// A connection attached, in place of any attached before -- which is told no more, and
    /// ends: which it is, where what the shell writes is shown to it, and what it wrote so far.
    pub fn attach(&self) -> (u64, mpsc::UnboundedReceiver<Shown>, String) {
        let which = self.attaches.fetch_add(1, Ordering::Relaxed);
        let (to, shown) = mpsc::unbounded_channel();
        // Under the backlog's lock, so that nothing written is shown twice or not at all.
        let backlog = self.backlog.lock().unwrap();
        *self.attached.lock().unwrap() = Some((which, to));
        let written = backlog.0.iter().map(String::as_str).collect();
        (which, shown, written)
    }

    /// Connection `which` gone, if it is the one attached: the shell left running.
    pub fn detach(&self, which: u64) {
        let mut attached = self.attached.lock().unwrap();
        if attached.as_ref().is_some_and(|(it, _)| *it == which) {
            *attached = None;
        }
    }

    /// `data` typed into the shell: done once the guest has written it, so a shell that does
    /// not read holds back the connection typing into it.
    pub async fn write(&self, data: String) -> Result<(), Refused> {
        let typing = self.terminal.lock().unwrap().as_ref().map(|t| t.typing());
        match typing {
            Some(typing) => Ok(typing.write(data).await?),
            None => Ok(()),
        }
    }

    /// The terminal made `cols` by `rows`.
    pub fn resize(&self, cols: u16, rows: u16) -> Result<(), Refused> {
        match &*self.terminal.lock().unwrap() {
            Some(terminal) => Ok(terminal.resize(cols, rows)?),
            None => Ok(()),
        }
    }

    /// The shell killed: the session ends with it.
    pub fn end(&self) {
        drop(self.terminal.lock().unwrap().take());
    }
}
