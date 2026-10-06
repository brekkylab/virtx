//! Keeps a mount from changing the process's `SIGCHLD` disposition.
//!
//! libfuse-t resets `SIGCHLD` to `SIG_DFL` before forking its mount helper and never
//! restores it. The disposition is process-wide, and Tokio installs its handler only
//! once, on the first child it spawns, so after a mount that follows a spawn every
//! `Child::wait` waits for a signal that never arrives. The reset happens inside the
//! library, so [`Sigchld`] restores the disposition afterwards instead.

/// Holds the `SIGCHLD` disposition across a mount and restores it on drop.
///
/// A guard so the restore runs on every exit from the mount, `?` and panic included.
/// Scope it to the mount call alone: the handler stays removed until it drops.
pub(crate) struct Sigchld(Option<libc::sigaction>);

impl Sigchld {
    /// Reads the current disposition, to restore on drop.
    pub(crate) fn held() -> Self {
        Self(disposition())
    }
}

impl Drop for Sigchld {
    fn drop(&mut self) {
        let (Some(was), Some(now)) = (self.0, disposition()) else {
            return;
        };
        if now.sa_sigaction == was.sa_sigaction && now.sa_flags == was.sa_flags {
            return;
        }
        // Installed by someone else during the mount: newer than `was`, so it stays.
        if now.sa_sigaction != libc::SIG_DFL && now.sa_sigaction != libc::SIG_IGN {
            return;
        }
        // SAFETY: `was` came from `sigaction`, and `kill` targets this process.
        unsafe {
            libc::sigaction(libc::SIGCHLD, &was, std::ptr::null_mut());
            // Resend the SIGCHLD of a child that exited during the mount, which hit
            // SIG_DFL and was lost; a spurious one is harmless since SIGCHLD
            // coalesces. `kill` rather than `raise`: a thread-directed signal stays
            // pending if this thread blocks SIGCHLD.
            if was.sa_sigaction != libc::SIG_DFL && was.sa_sigaction != libc::SIG_IGN {
                libc::kill(libc::getpid(), libc::SIGCHLD);
            }
        }
    }
}

/// The current `SIGCHLD` disposition; `None` only if `sigaction` fails, which it
/// cannot for `SIGCHLD`. A guard that read nothing restores nothing.
fn disposition() -> Option<libc::sigaction> {
    // SAFETY: `current` is written before it is read, and a null `act` installs nothing.
    unsafe {
        let mut current: libc::sigaction = std::mem::zeroed();
        (libc::sigaction(libc::SIGCHLD, std::ptr::null(), &mut current) == 0).then_some(current)
    }
}
