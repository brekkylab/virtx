//! `virtx-uvm-guest` -- the agent inside each of virtx-uvm's machines.
//!
//! libkrun's init has already built the root -- the image under the machine's ext4, the shares
//! mounted -- and execs this standing where the image said. What is left is to open the
//! channel, say the guest is up, and run the commands the host sends side by side, each in a
//! process group of its own, sending what they write a line at a time.
//!
//! The channel is a named virtio-console port, not stdin and stdout: those are the guest's
//! console, where the kernel prints, and one kernel warning would corrupt a frame there.

use std::fs::File;
use std::io::{self, BufRead as _, BufReader, Read};
use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
use std::path::PathBuf;
use std::process::{Child, Command, ExitCode, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;

#[path = "../../virtx-uvm/src/machine/wire.rs"]
mod wire;

use wire::{HostMessage, MachineMessage, PORT_NAME};

/// The port's writing half, shared by everything that says something on it, so two messages
/// never interleave.
type Out = Arc<Mutex<File>>;

/// The most of a line one message carries: escaped as JSON, a byte is at most six, well within
/// a frame.
const LINE_LIMIT: u64 = 64 << 10;

fn main() -> ExitCode {
    match serve() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // To the console, which the host keeps as the machine's log.
            eprintln!("{}: {e}", env!("CARGO_BIN_NAME"));
            ExitCode::FAILURE
        }
    }
}

fn serve() -> io::Result<()> {
    let path = find().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("no virtio-console port named `{PORT_NAME}`"),
        )
    })?;
    // One descriptor, split in two: the driver allows a single opener per port.
    let mut input = File::options().read(true).write(true).open(&path)?;
    let out: Out = Arc::new(Mutex::new(input.try_clone()?));

    send(&out, &MachineMessage::Ready(Vec::new()));
    while let Some(message) = wire::recv(&mut input)? {
        match message {
            HostMessage::Exec { cmd } => exec(&out, cmd),
            // SAFETY: `kill` takes plain values; a group that is gone is an error it returns.
            HostMessage::Signal { id, signal } => unsafe {
                libc::kill(-(id as libc::pid_t), signal);
            },
            // As libkrun's init ends the machine once its workload has.
            // SAFETY: neither takes anything but plain values.
            HostMessage::Shutdown => unsafe {
                libc::sync();
                libc::reboot(libc::RB_AUTOBOOT);
            },
        }
    }
    Ok(())
}

fn find() -> Option<PathBuf> {
    let by_name = PathBuf::from("/dev/virtio-ports").join(PORT_NAME);
    if by_name.exists() {
        return Some(by_name);
    }
    for entry in std::fs::read_dir("/sys/class/virtio-ports").ok()?.flatten() {
        let named = std::fs::read_to_string(entry.path().join("name")).unwrap_or_default();
        if named.trim() == PORT_NAME {
            return Some(PathBuf::from("/dev").join(entry.file_name()));
        }
    }
    None
}

/// Start `cmd` in a process group of its own, answer the host with its pid -- before anything
/// it writes, which goes after -- and send how it ended once it has.
fn exec(out: &Out, cmd: Vec<String>) {
    let started = Command::new(&cmd[0])
        .args(&cmd[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn();
    let mut child = match started {
        Ok(child) => child,
        Err(e) => {
            send(
                out,
                &MachineMessage::Started(Err(format!("{}: {e}", cmd[0]))),
            );
            return;
        }
    };
    let id = child.id();
    send(out, &MachineMessage::Started(Ok(id)));

    let stdout = lines(out, child.stdout.take(), move |line| {
        MachineMessage::Stdout { id, line }
    });
    let stderr = lines(out, child.stderr.take(), move |line| {
        MachineMessage::Stderr { id, line }
    });
    let out = Arc::clone(out);
    thread::spawn(move || {
        // Everything it wrote first, so its end is the last thing said of it.
        let _ = (stdout.join(), stderr.join());
        send(
            &out,
            &MachineMessage::Exited {
                id,
                code: code(&mut child),
            },
        );
    });
}

/// Each line `read` gives, as `wrapped` makes it a message, until it ends. One longer than
/// [`LINE_LIMIT`] goes as several, so no frame is past what the host reads.
fn lines(
    out: &Out,
    read: Option<impl Read + Send + 'static>,
    wrapped: impl Fn(String) -> MachineMessage + Send + 'static,
) -> thread::JoinHandle<()> {
    let out = Arc::clone(out);
    thread::spawn(move || {
        let Some(read) = read else { return };
        let mut read = BufReader::new(read);
        let mut line = Vec::new();
        loop {
            line.clear();
            match (&mut read).take(LINE_LIMIT).read_until(b'\n', &mut line) {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
            if line.last() == Some(&b'\n') {
                line.pop();
            }
            send(&out, &wrapped(String::from_utf8_lossy(&line).into_owned()));
        }
    })
}

/// How `child` ended: its exit code, `128 + signal` for one a signal ended.
fn code(child: &mut Child) -> i32 {
    match child.wait() {
        Ok(status) => status
            .code()
            .or(status.signal().map(|signal| 128 + signal))
            .unwrap_or(-1),
        Err(_) => -1,
    }
}

/// Tell the host `message`. A port that will not take it is a host that is gone, which hears
/// nothing more either way.
fn send(out: &Out, message: &MachineMessage) {
    let _ = wire::send(&mut *out.lock().unwrap(), message);
}
