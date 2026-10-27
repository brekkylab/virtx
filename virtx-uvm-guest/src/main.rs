//! `virtx-uvm-guest` -- the init inside each of virtx-uvm's machines.
//!
//! libkrun's init has already built the root -- the image under the machine's ext4, the shares
//! mounted -- and execs this standing where the image said. What is left is to open the
//! channel and run the commands the host sends, each in a process group
//! of its own, sending what they write a line at a time.
//!
//! The channel is a named virtio-console port, not stdin and stdout: those are the guest's
//! console, where the kernel prints, and one kernel warning would corrupt a frame there.

use std::collections::HashMap;
use std::io;
use std::os::fd::{AsRawFd as _, FromRawFd as _};
use std::os::unix::process::ExitStatusExt as _;
use std::path::PathBuf;
use std::process::{ExitCode, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncReadExt as _};
use tokio::process::Command;
use tokio::sync::mpsc;
#[path = "../../virtx-uvm/src/machine/wire.rs"]
mod wire;

use wire::{FromGuest, HANDSHAKE, PORT_NAME, ToGuest, recv, send};

/// The longest line sent: a longer one is sent in pieces this long.
const MAX_LINE: usize = 64 << 10;

/// Each command not yet ended, by the id of the `Exec` that runs it: its process group once it
/// has one. A `Kill` takes it out, so one that has no group yet is killed as it gets one.
type Running = Arc<Mutex<HashMap<u64, Option<libc::pid_t>>>>;

fn main() -> ExitCode {
    let served = open().and_then(|port| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?
            .block_on(serve(port))
    });
    match served {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // To the console, which the host keeps as the boot's log.
            eprintln!("{}: {e:#}", env!("CARGO_BIN_NAME"));
            ExitCode::FAILURE
        }
    }
}

/// The channel's port, once the kernel has made it.
fn open() -> anyhow::Result<std::fs::File> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(path) = find() {
            return std::fs::File::options()
                .read(true)
                .write(true)
                .open(&path)
                .map_err(|e| anyhow::anyhow!("opening the port {}: {e}", path.display()));
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "no virtio-console port named `{PORT_NAME}` appeared"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn find() -> Option<PathBuf> {
    let by_name = PathBuf::from("/dev/virtio-ports").join(PORT_NAME);
    if by_name.exists() {
        return Some(by_name);
    }
    for entry in std::fs::read_dir("/sys/class/virtio-ports").ok()?.flatten() {
        let named = std::fs::read_to_string(entry.path().join("name")).unwrap_or_default();
        let node = PathBuf::from("/dev").join(entry.file_name());
        if named.trim() == PORT_NAME && node.exists() {
            return Some(node);
        }
    }
    None
}

async fn serve(port: std::fs::File) -> anyhow::Result<()> {
    // Two halves of one descriptor: the driver allows a single opener per port.
    // SAFETY: `dup` hands back a fresh descriptor for the same open file, owned by the `File`
    // built from it and nothing else.
    let duplicate = unsafe { libc::dup(port.as_raw_fd()) };
    anyhow::ensure!(
        duplicate >= 0,
        "duplicating the port: {}",
        io::Error::last_os_error()
    );
    let mut outgoing = tokio::fs::File::from_std(unsafe { std::fs::File::from_raw_fd(duplicate) });
    let mut incoming = tokio::fs::File::from_std(port);

    tokio::io::AsyncWriteExt::write_all(&mut outgoing, HANDSHAKE).await?;

    // One writer, so two calls' messages never interleave on the port.
    let (sender, mut queued) = mpsc::unbounded_channel::<FromGuest>();
    let writer = tokio::spawn(async move {
        while let Some(message) = queued.recv().await {
            send(&mut outgoing, &message).await?;
        }
        io::Result::Ok(())
    });

    let running = Running::default();
    while let Some(call) = recv::<_, ToGuest>(&mut incoming).await? {
        let sender = sender.clone();
        match call {
            ToGuest::Exec {
                id,
                cmd,
                timeout_ms,
            } => {
                // Before anything else is read, so a `Kill` after it finds it.
                running.lock().unwrap().insert(id, None);
                let running = Arc::clone(&running);
                tokio::spawn(async move {
                    execute(id, cmd, timeout_ms, &running, &sender).await;
                    running.lock().unwrap().remove(&id);
                });
            }
            ToGuest::Sync { id } => {
                tokio::spawn(async move {
                    // SAFETY: `sync(2)` takes nothing and cannot fail; it returns once the
                    // writes have reached the devices.
                    let _ = tokio::task::spawn_blocking(|| unsafe { libc::sync() }).await;
                    let _ = sender.send(FromGuest::Synced { id });
                });
            }
            ToGuest::Kill { id } => {
                if let Some(Some(group)) = running.lock().unwrap().remove(&id) {
                    // SAFETY: `group` is the command's own, still: its leader is reaped only
                    // after `execute` has taken it out of `running`, under this lock.
                    unsafe { libc::killpg(group, libc::SIGKILL) };
                }
            }
        }
    }
    drop(sender);
    writer.await??;
    Ok(())
}

/// Run `cmd`, sending each line it writes, then how it ended.
async fn execute(
    id: u64,
    cmd: Vec<String>,
    timeout_ms: Option<u64>,
    running: &Running,
    sender: &mpsc::UnboundedSender<FromGuest>,
) {
    let Some((program, args)) = cmd.split_first() else {
        let _ = sender.send(FromGuest::Failed {
            id,
            message: "an empty command".into(),
        });
        return;
    };
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        // Its own group, so one signal reaches everything it starts, and never this process,
        // the guest's PID 1.
        .process_group(0)
        // This process's environment, which is the image's, less the init's `KRUN_` plumbing.
        .env_clear()
        .envs(
            std::env::vars_os()
                .filter(|(name, _)| !name.as_encoded_bytes().starts_with(b"KRUN_")),
        );
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => {
            let _ = sender.send(FromGuest::Failed {
                id,
                message: format!("{program}: {e}"),
            });
            return;
        }
    };
    // Taken now: it is also the group's id, and stays this command's until it is reaped.
    let pid = child.id();
    if let Some(pid) = pid {
        let pid = pid as libc::pid_t;
        match running.lock().unwrap().get_mut(&id) {
            Some(group) => *group = Some(pid),
            // Killed before it had a group to kill.
            // SAFETY: `pid` is the command just spawned, not yet reaped.
            None => unsafe {
                libc::killpg(pid, libc::SIGKILL);
            },
        }
    }

    let (stdout, stderr) = (child.stdout.take(), child.stderr.take());
    // Both pipes to their end, then the reap: while anything the command started still writes,
    // the leader stays unreaped and its pid -- the group's -- stays this command's.
    let ran = async {
        tokio::try_join!(lines(id, stdout, sender), lines(id, stderr, sender))?;
        // Out of `running` before the reap, so a `Kill` never signals a group that is gone.
        running.lock().unwrap().remove(&id);
        child.wait().await
    };
    let ended = match timeout_ms {
        None => ran.await,
        Some(ms) => match tokio::time::timeout(Duration::from_millis(ms), ran).await {
            Ok(ended) => ended,
            Err(_) => {
                running.lock().unwrap().remove(&id);
                if let Some(pid) = pid {
                    // SAFETY: `pid` names this command's group, which is still its own: the
                    // leader has not been reaped (see above).
                    unsafe { libc::killpg(pid as libc::pid_t, libc::SIGKILL) };
                }
                let _ = sender.send(FromGuest::TimedOut { id });
                return;
            }
        },
    };
    let _ = sender.send(match ended {
        Ok(status) => FromGuest::Exited {
            id,
            code: status
                .code()
                .or_else(|| status.signal().map(|signal| 128 + signal))
                .unwrap_or(126),
        },
        Err(e) => FromGuest::Failed {
            id,
            message: format!("waiting for {program}: {e}"),
        },
    });
}

/// Send each line `pipe` gives as it comes, and what is left of one at its end.
async fn lines(
    id: u64,
    pipe: Option<impl AsyncRead + Unpin>,
    sender: &mpsc::UnboundedSender<FromGuest>,
) -> io::Result<()> {
    let Some(mut pipe) = pipe else {
        return Ok(());
    };
    let send = |bytes: &[u8]| {
        let line = String::from_utf8_lossy(bytes);
        let _ = sender.send(FromGuest::Line {
            id,
            line: line.trim_end_matches('\r').to_owned(),
        });
    };
    let mut pending = Vec::new();
    let mut chunk = [0; 8192];
    loop {
        let read = pipe.read(&mut chunk).await?;
        if read == 0 {
            if !pending.is_empty() {
                send(&pending);
            }
            return Ok(());
        }
        pending.extend_from_slice(&chunk[..read]);
        while let Some(end) = pending.iter().position(|&b| b == b'\n') {
            send(&pending[..end]);
            pending.drain(..=end);
        }
        while pending.len() >= MAX_LINE {
            send(&pending[..MAX_LINE]);
            pending.drain(..MAX_LINE);
        }
    }
}
