//! `cortex-krun-guest` -- the half that runs inside the guest.
//!
//! Built for the guest's target rather than this host's, which is why it is its own binary
//! and not a flag on the server's.
//!
//! # What it does not do
//!
//! Build a root. libkrun's init mounts the image under this session's ext4, pivots onto the
//! overlay, mounts the shares the boot attached and execs this binary standing where the
//! image said -- all of it before this process exists. So there is no `mount(2)` here, no
//! `pivot_root`, and nothing to undo: what this process is handed is already the root the
//! commands will run on.
//!
//! **And no layer to hand back.** A session's writes are on its ext4, which is a file on the
//! host that the server made and keeps -- carrying a session on is keeping that file and
//! throwing one away is deleting it. There is nothing here for a commit to walk.
//!
//! # The channel is a virtio port, not stdin and stdout
//!
//! Stdin and stdout in here belong to the guest's console, which is where the kernel prints
//! and where a command's own output would go if anything let it through. Putting frames on
//! that would mean a protocol stream a single kernel warning could corrupt.
//!
//! So the session runs over a named virtio-console port --
//! [`PORT_NAME`](cortex_krun_host::PORT_NAME) -- opened here before the session starts. Two
//! halves from one descriptor, because the driver allows a single opener per port: [`serve`]
//! `dup`s the handle rather than opening the device twice.
//!
//! The first thing on it is [`HANDSHAKE`](cortex_krun_host::HANDSHAKE), sent by this end. It
//! is not part of the protocol and it is not politeness -- a virtio-console port discards
//! what the host writes while no process in the guest has it open, so the host has to be told
//! when reading has started, and there is nothing in the protocol for this end to say first.

mod port;
mod session;

use std::process::ExitCode;

/// A failure of ours, not a command's -- the shell's code for "found it, could not run it".
const NOT_EXECUTABLE: u8 = 126;

/// Not async at the top, and the runtime is built by hand rather than by `#[tokio::main]`:
/// the port is found and opened with blocking calls, before there is anything to wait on,
/// and this process is the guest's PID 1 -- one thread until there is a reason for more.
fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // To the console, which is the guest's own: a failure in here is one the server
            // reads out of the log it redirected the boot's stdio into.
            eprintln!("{}: {e:#}", env!("CARGO_BIN_NAME"));
            ExitCode::from(NOT_EXECUTABLE)
        }
    }
}

/// The one argument this binary takes, and the one thing it does that is not a session.
///
/// A build reads the machine's disk **on the host**, after the machine is gone. What the
/// guest has written is not on that disk until the guest has flushed it, and the server
/// takes a machine down by killing it -- so something inside has to flush first, and the
/// only thing certain to be in there is this binary. The server runs it as a command,
/// through the same `exec` a client's commands go through.
///
/// Not `sync(1)`: an image is not obliged to carry coreutils, and a build that failed
/// because the base was slim would be failing for the wrong reason.
const SYNC: &str = "sync";

fn run() -> anyhow::Result<()> {
    if std::env::args().nth(1).as_deref() == Some(SYNC) {
        // SAFETY: `sync(2)` takes no arguments, returns nothing, cannot fail and changes
        // nothing about this process. It returns once the writes have reached the device,
        // which is what the host is waiting for.
        unsafe { libc::sync() };
        return Ok(());
    }

    let port = port::open()?;
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(session::serve(port))
}
