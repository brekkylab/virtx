//! `virtx-uvm` -- the half that answers a console session.
//!
//! Reads requests on stdin and writes answers on stdout, per [`virtx::protocol`]; what it
//! does with one is to run it inside a micro-VM. The VM is not in this process: a session
//! spawns `virtx-uvm-host`, the signed binary sitting beside this one, and talks to the
//! guest down a socket that boot turns into a console port. See [`session::halves`] for why
//! the hypervisor is somewhere else and how the three files find each other.
//!
//! A client spawns this binary to be its server and passes nothing, which is why no arguments
//! is the session rather than a usage message: the common case is the one a caller does not
//! have to spell.

mod image;
mod session;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

/// A failure of ours, not a command's -- the shell's code for "found it, could not run it".
const NOT_EXECUTABLE: u8 = 126;

/// Everything this server keeps on disk, under one root -- `$VIRTX_HOME`, or
/// `virtx` under the user's cache directory. The rule is
/// [`virtx::cache_root`]'s, so that this and the images below it are one answer to where
/// this host keeps things rather than two.
///
/// ```text
/// {home}/
/// ├── abin/                   virtx's own executables, shared into every guest at
/// │   │                       `/abin` when there is no `abin/` beside this binary --
/// │   │                       a directory this server reads and never writes
/// │   └── skills/{program}/   what each of them is, as the session reads it
/// ├── images/                 the images sessions boot from, shared by every tool in
/// │                           this repository -- `Cache`'s, not this server's
/// └── session/                one directory per live server process
///     └── {pid}/
///         ├── session.ext4    what the session wrote -- outlives any one machine
///         └── machine/        what only makes sense while one is up
///             ├── guest/      shared into the guest: the guest binary it execs
///             ├── port.sock   the console channel, where a host has one as a file
///             └── console.log the kernel's own output
/// ```
///
/// `abin/` is the one thing here that is not a session's: it outlives every process, is the
/// same for all of them, and a host that has none is a host whose sessions get no `/abin` --
/// see `ABIN_DIR`.
///
/// `images/` is under this root and is not this server's: it is
/// [`Cache`](virtx_uvm::image::Cache)'s, every tool in this repository reads and writes
/// it, and `VIRTX_IMAGE_CACHE_ROOT` moves it out from under here on its own. What is left
/// is one process's own, split by lifetime so that each level's cleanup is one call: a
/// `stop` removes `machine/`, and the end of a session removes `{pid}/`.
///
/// A host with nowhere to keep this is a panic and not a temporary directory, unlike the
/// images: `session.ext4` is what a session wrote, and a server that put it somewhere the
/// machine may sweep would be losing it rather than saying it had nowhere to put it.
pub(crate) fn home() -> PathBuf {
    virtx::cache_root()
}

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Answer a console session on stdin and stdout (the default).
    Run,
    /// Manage the images sessions boot from.
    #[command(subcommand)]
    Image(image::Command),
}

#[tokio::main]
async fn main() -> ExitCode {
    let (result, failure) = match Cli::parse().command {
        None | Some(Command::Run) => (session::run().await, NOT_EXECUTABLE),
        Some(Command::Image(command)) => (image::run(command).await, 1),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // To stderr and never stdout: from the moment the server starts, stdout carries
            // frames and nothing else.
            eprintln!("{}: {e:#}", env!("CARGO_BIN_NAME"));
            ExitCode::from(failure)
        }
    }
}
