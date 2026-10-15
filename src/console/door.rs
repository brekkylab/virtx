//! A port on this host's loopback that sessions reach.
//!
//! A session's commands do not reach this host's loopback: what listens there is the
//! operator's. A port a session published is the exception, since it was opened for something
//! to come in, and the server learns which ports those are from files under
//! [`cache_root`]`/session`: one directory per owner, a `ports` file in it naming the ports a
//! line apiece, honoured only while `ports.lock` beside it is held. A [`Door`] is one such
//! directory for a listener of this process's own.

use std::{
    fs::File,
    io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use crate::cache_root;

const PORTS: &str = "ports";
const LOCK: &str = "ports.lock";
/// Not a number, which a server would take for its own pid and sweep.
const PREFIX: &str = "door-";

/// `port` on this host's loopback, reachable as `127.0.0.1:<port>` from every session with its
/// network on, for as long as this is held.
///
/// Anything in any session can then connect, so a listener behind a door checks who it is
/// talking to, as a published port's does.
///
/// ```no_run
/// # fn main() -> std::io::Result<()> {
/// let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
/// let door = virtx::console::Door::open(listener.local_addr()?.port())?;
/// // A session's `curl http://127.0.0.1:<port>` now reaches `listener`, until `door` drops.
/// # Ok(())
/// # }
/// ```
#[must_use = "the port is reachable only while the Door is held"]
pub struct Door {
    dir: PathBuf,
    port: u16,
    /// Held, never read: the lock is what vouches for `ports`. `None` once dropped.
    lock: Option<File>,
}

impl Door {
    pub fn open(port: u16) -> io::Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let sessions = cache_root().join("session");
        sweep(&sessions);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = sessions.join(format!("{PREFIX}{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        // Locked before `ports` is written, so a reader never finds the file unvouched for.
        let lock = File::create(dir.join(LOCK))?;
        lock.lock()?;
        let partial = dir.join(format!("{PORTS}.partial"));
        std::fs::write(&partial, format!("{port}\n"))?;
        std::fs::rename(partial, dir.join(PORTS))?;
        Ok(Self {
            dir,
            port,
            lock: Some(lock),
        })
    }

    pub fn port(&self) -> u16 {
        self.port
    }
}

impl Drop for Door {
    fn drop(&mut self) {
        // `ports` before the lock, so no reader sees it unvouched for; the directory after,
        // which Windows refuses while the lock file is open.
        let _ = std::fs::remove_file(self.dir.join(PORTS));
        drop(self.lock.take());
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Removes doors whose owner died without closing them: nobody holds their lock.
fn sweep(sessions: &Path) {
    let Ok(entries) = std::fs::read_dir(sessions) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().starts_with(PREFIX) && !held(&entry.path()) {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// Whether someone holds `dir`'s lock: a lock asked for beside it is refused.
fn held(dir: &Path) -> bool {
    File::open(dir.join(LOCK)).is_ok_and(|file| {
        matches!(
            file.try_lock_shared(),
            Err(std::fs::TryLockError::WouldBlock)
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_door_names_its_port_while_held() {
        let door = Door::open(3977).unwrap();
        let dir = door.dir.clone();
        assert!(
            dir.file_name()
                .unwrap()
                .to_string_lossy()
                .parse::<u32>()
                .is_err()
        );
        assert_eq!(std::fs::read_to_string(dir.join(PORTS)).unwrap(), "3977\n");
        assert!(held(&dir));
        // Another door in this process neither takes it nor sweeps it.
        let other = Door::open(3978).unwrap();
        assert_ne!(other.dir, dir);
        assert!(dir.join(PORTS).exists());

        drop(door);
        assert!(!dir.exists());
        drop(other);
    }

    #[test]
    fn an_unheld_door_is_swept() {
        let sessions = cache_root().join("session");
        let stale = sessions.join(format!("{PREFIX}0-test-{}", std::process::id()));
        std::fs::create_dir_all(&stale).unwrap();
        std::fs::write(stale.join(PORTS), "3979\n").unwrap();
        File::create(stale.join(LOCK)).unwrap();
        // A server's own directory is a pid, never a door's to sweep.
        let server = sessions.join("4000000");
        std::fs::create_dir_all(&server).unwrap();

        drop(Door::open(3980).unwrap());
        assert!(!stale.exists());
        assert!(server.exists());
        std::fs::remove_dir_all(server).unwrap();
    }
}
