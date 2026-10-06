//! A mount must leave the process's `SIGCHLD` disposition alone. Tokio's
//! `Child::wait` depends on its handler, so a mount that resets it leaves every
//! later wait unanswered even though the child has exited.
//!
//! `#[ignore]` because these mount real filesystems:
//!
//! ```sh
//! cargo test --test mount_sigchld -- --ignored --nocapture
//! ```

#![cfg(all(feature = "mount", unix))]

use std::{fs, path::PathBuf, time::Duration};

#[cfg(not(target_os = "macos"))]
use virtx::fs::FuseMount as HostMount;
#[cfg(target_os = "macos")]
use virtx::fs::FuseTMount as HostMount;
use virtx::fs::InMemFs;

/// Upper bound for reaping a child that has already exited; generous so a loaded
/// machine isn't mistaken for a lost signal.
const REAP_BUDGET: Duration = Duration::from_secs(3);

/// Only ends a hung run; the assertion is on [`REAP_BUDGET`]. An expiring `timeout`
/// re-polls the wait, which then reaps via `try_wait`, so a lost signal still comes
/// back `Ok` here, just late.
const GIVE_UP: Duration = Duration::from_secs(15);

fn mountpoint(tag: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    dir.push(format!("virtx-sigchld-{}-{}", std::process::id(), tag));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("temp dir is writable");
    dir
}

/// The current `SIGCHLD` handler and flags, for failure messages.
#[derive(Debug, PartialEq, Eq)]
struct Disposition {
    handler: usize,
    flags: libc::c_int,
}

impl Disposition {
    fn read() -> Self {
        // SAFETY: `current` is written before it is read, and a null `act` installs nothing.
        let current = unsafe {
            let mut current: libc::sigaction = std::mem::zeroed();
            assert_eq!(
                libc::sigaction(libc::SIGCHLD, std::ptr::null(), &mut current),
                0,
                "reading a signal disposition cannot fail for a valid signal"
            );
            current
        };
        Self {
            handler: current.sa_sigaction,
            flags: current.sa_flags,
        }
    }

    fn describe(&self) -> String {
        let handler = match self.handler {
            h if h == libc::SIG_DFL => "SIG_DFL".to_string(),
            h if h == libc::SIG_IGN => "SIG_IGN".to_string(),
            h => format!("a handler at {h:#x}"),
        };
        format!("{handler}, flags {:#x}", self.flags)
    }
}

/// A child spawned after a mount is still reaped promptly.
#[test]
#[ignore = "needs a host binding and mounts real filesystems"]
fn a_child_is_still_reaped_after_a_mount() {
    let rt = tokio::runtime::Runtime::new().expect("build a runtime");

    // Installs Tokio's SIGCHLD handler; without a prior spawn the mount has nothing to reset.
    rt.block_on(async {
        tokio::process::Command::new("true")
            .spawn()
            .expect("spawn `true`")
            .wait()
            .await
            .expect("a child that exits is reaped");
    });

    let _mount = HostMount::try_new(InMemFs::new(), &mountpoint("reap"))
        .expect("the host binding can mount");

    let started = std::time::Instant::now();
    let reaped = rt.block_on(async {
        let mut child = tokio::process::Command::new("true")
            .spawn()
            .expect("spawn `true` again");
        tokio::time::timeout(GIVE_UP, child.wait()).await
    });

    let took = started.elapsed();

    reaped
        .expect("the wait gave no sign of returning on its own")
        .expect("waiting on a child that exited cleanly");
    assert!(
        took < REAP_BUDGET,
        "a child that had already exited took {took:?} to be reaped after a mount, \
         which is the wait not returning until something else woke it. SIGCHLD is \
         now {}: the mount took the notification the runtime waits on.",
        Disposition::read().describe()
    );
}

/// Mounting before the first spawn protects only that mount: a later one resets the
/// handler the earlier spawn installed.
#[test]
#[ignore = "needs a host binding and mounts real filesystems"]
fn a_second_session_does_not_break_the_first_session_s_waits() {
    let rt = tokio::runtime::Runtime::new().expect("build a runtime");

    // Session one, in the safe order: mount, then spawn.
    let first = HostMount::try_new(InMemFs::new(), &mountpoint("session-one"))
        .expect("the host binding can mount");
    rt.block_on(async {
        tokio::process::Command::new("true")
            .spawn()
            .expect("spawn `true`")
            .wait()
            .await
            .expect("a child that exits is reaped");
    });
    drop(first);

    // Session two also mounts before spawning; it is the process's second mount.
    let _second = HostMount::try_new(InMemFs::new(), &mountpoint("session-two"))
        .expect("the host binding can mount again");

    let started = std::time::Instant::now();
    let reaped = rt.block_on(async {
        let mut child = tokio::process::Command::new("true")
            .spawn()
            .expect("spawn `true` in the second session");
        tokio::time::timeout(GIVE_UP, child.wait()).await
    });
    let took = started.elapsed();

    reaped
        .expect("the wait gave no sign of returning on its own")
        .expect("waiting on a child that exited cleanly");
    assert!(
        took < REAP_BUDGET,
        "the second session's wait took {took:?}. Mounting once at startup did not \
         protect the process: the second mount took the notification back out. \
         SIGCHLD is now {}.",
        Disposition::read().describe()
    );
}
