//! Which ports on this host lead into a session: what every server here has published, where
//! any session can read it.
//!
//! # Why a guest needs to know
//!
//! A published port is a listener on this host's loopback, and a guest's own loopback is
//! refused -- see [`net`](super::net). But a published port is not the operator's: a server
//! opened it so that something could come in, and a program in *another* session is as
//! much something as a viewer on this host. So a guest's `connect` to `127.0.0.1:<port>` is
//! let through when that port is one a live session published, and only then.
//!
//! # Where it is written
//!
//! Each server runs one session, in a directory of its own named by its pid under
//! [`sessions`], and writes the host ports it published to a `ports` file there -- one a
//! line, by a rename so that a reader sees all of them or none. The directory goes when the
//! session does, and one whose owner died is ignored until a later server sweeps it: a port
//! is published while the process that recorded it holds its [`Record`].
//!
//! **A lock, not a pid.** What a [`Record`] holds is a lock on a file beside `ports`, and
//! the operating system lets go of it when the process dies, however it dies. A pid would
//! not do here: one can be reused, and a crashed server's `ports` under a reused pid would
//! let every guest reach whatever the operator later binds on that port. That is safe for
//! [`alive`], which only decides what a sweep may delete, and not for this, which grants
//! access.

use std::io;
use std::path::{Path, PathBuf};

/// The file in a session's directory that names its published host ports.
const PORTS: &str = "ports";

/// The file a session holds locked while its ports are published. Apart from [`PORTS`],
/// because a lock on Windows is mandatory: one on `ports` itself would keep every other
/// session from reading it.
const LOCK: &str = "ports.lock";

/// Where the servers on this host keep their sessions: one directory apiece, named by the
/// pid of the process that owns it.
pub fn sessions() -> PathBuf {
    virtx::cache_root().join("session")
}

/// Say that the session in `dir` published `hosts`, the ports on this host's loopback that
/// lead into it, for as long as the [`Record`] this answers is held.
///
/// The lock is taken before `ports` is written, so a reader that finds the file finds it
/// held -- unless its owner has since died, which is what the lock is there to tell.
pub fn record(dir: &Path, hosts: &[u16]) -> io::Result<Record> {
    std::fs::create_dir_all(dir)?;
    let lock = std::fs::File::create(dir.join(LOCK))?;
    lock.lock()?;
    let text: String = hosts.iter().map(|port| format!("{port}\n")).collect();
    let partial = dir.join(format!("{PORTS}.partial"));
    std::fs::write(&partial, text)?;
    std::fs::rename(partial, dir.join(PORTS))?;
    Ok(Record {
        dir: dir.to_path_buf(),
        _lock: lock,
    })
}

/// A session's published ports, published for as long as this is held. Dropped, it takes
/// `ports` away and lets go of the lock -- so a session that fails after recording, and
/// goes on running, does not leave its ports open to every guest.
#[must_use = "the ports are published only while the Record is held"]
pub struct Record {
    dir: PathBuf,
    /// Held, never read: what it is for is the lock, which closing it releases.
    _lock: std::fs::File,
}

impl Drop for Record {
    fn drop(&mut self) {
        // `ports` before the lock, which goes with `_lock` after this: a reader never sees
        // the file without the lock that vouches for it.
        let _ = std::fs::remove_file(self.dir.join(PORTS));
    }
}

/// Whether the session in `dir` still holds its [`Record`]: someone else has `ports.lock`
/// locked, which a lock asked for alongside it is refused for.
fn held(dir: &Path) -> bool {
    std::fs::File::open(dir.join(LOCK)).is_ok_and(|file| {
        matches!(
            file.try_lock_shared(),
            Err(std::fs::TryLockError::WouldBlock)
        )
    })
}

/// Whether `port` on this host's loopback is one a live session published.
///
/// Read afresh each time, because sessions come and go while a guest runs. It is asked only
/// for a loopback `connect` the guest's own stack had nothing for, which is rare.
pub(crate) fn published(port: u16) -> bool {
    let Ok(entries) = std::fs::read_dir(sessions()) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let Ok(text) = std::fs::read_to_string(entry.path().join(PORTS)) else {
            return false;
        };
        text.lines().any(|line| line.trim().parse() == Ok(port)) && held(&entry.path())
    })
}

/// Whether a process is still running under `pid`.
///
/// **"Gone" and not "not mine"**, which is the whole difficulty: a pid that answers is one
/// whose session directory has an owner, and a pid that is merely out of this user's reach
/// answers too. Getting that backwards would delete a live session's files.
///
/// `kill(pid, 0)` sends no signal and performs only the permission and existence check, so
/// `ESRCH` is the one answer that means gone and `EPERM` says alive under another user.
/// Windows has no such call: `OpenProcess` for the least it can be opened for either answers
/// a handle, or refuses with `ERROR_ACCESS_DENIED` for a process this user may not touch --
/// both of which are a process that is there. Only `ERROR_INVALID_PARAMETER`, which is what
/// it answers for a pid nothing is using, means gone.
///
/// Either way a pid can be reused, and then a dead owner looks alive and its directory is
/// left for a later run to meet again. That is the safe direction to be wrong in.
#[cfg(unix)]
pub fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 performs only that check; it cannot affect this or any other process.
    (unsafe { libc::kill(pid, 0) == 0 })
        || io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

#[cfg(windows)]
pub fn alive(pid: i32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_INVALID_PARAMETER};
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

    // SAFETY: a call that only asks for a handle. The handle is closed below and nothing is
    // done with the process itself.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid as u32) };
    if !handle.is_null() {
        // SAFETY: `handle` was just opened and is not used again.
        unsafe { CloseHandle(handle) };
        return true;
    }
    io::Error::last_os_error().raw_os_error() != Some(ERROR_INVALID_PARAMETER as i32)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A port is read back from the file it was recorded in, and only while its record is held.
    #[test]
    fn a_recorded_port_is_published_while_its_record_is_held() {
        let dir = std::env::temp_dir().join(format!("virtx-doors-{}", std::process::id()));
        let record = record(&dir, &[3977, 5901]).unwrap();
        let text = std::fs::read_to_string(dir.join(PORTS)).unwrap();
        assert_eq!(text, "3977\n5901\n");
        assert!(!dir.join(format!("{PORTS}.partial")).exists());
        assert!(held(&dir), "a held record's lock is refused to anyone else");

        drop(record);
        assert!(
            !dir.join(PORTS).exists(),
            "dropping the record takes `ports` away"
        );
        assert!(!held(&dir), "and lets go of the lock");
        std::fs::remove_dir_all(&dir).unwrap();

        assert!(alive(std::process::id() as i32));
    }
}
