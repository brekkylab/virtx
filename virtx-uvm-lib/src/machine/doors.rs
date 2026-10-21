//! Where the servers on this host keep their sessions, and whether the process that owns one
//! is still running.

use std::io;
use std::path::PathBuf;

/// Where the servers on this host keep their sessions: one directory apiece, named by the
/// pid of the process that owns it.
pub fn sessions() -> PathBuf {
    virtx::cache_root().join("session")
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
