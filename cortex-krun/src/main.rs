//! `cortex-krun` -- the half that answers a console session.
//!
//! Reads requests on stdin and writes answers on stdout, per [`cortex::console`]; what it
//! does with one is to run it inside a micro-VM. The VM is not in this process: a session
//! spawns `cortex-krun-host`, the signed binary sitting beside this one, and talks to the
//! guest down a socket that boot turns into a console port. See [`session::halves`] for why
//! the hypervisor is somewhere else and how the three files find each other.
//!
//! A client spawns this binary to be its server and passes nothing, which is why no arguments
//! is the session rather than a usage message: the common case is the one a caller does not
//! have to spell.

mod session;

use std::path::PathBuf;
use std::process::ExitCode;

/// A failure of ours, not a command's -- the shell's code for "found it, could not run it".
const NOT_EXECUTABLE: u8 = 126;

/// Everything this server keeps on disk, under one root -- `$CORTEX_KRUN_HOME`, or
/// `cortex-krun` under the user's cache directory. The rule is
/// [`cortex_krun::home`]'s, so that this and the images below it are one answer to where
/// this host keeps things rather than two.
///
/// ```text
/// {home}/
/// ├── abin/                   cortex's own executables, shared into every guest at
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
/// [`Cache`](cortex_krun::image::Cache)'s, every tool in this repository reads and writes
/// it, and `CORTEX_IMAGE_CACHE_ROOT` moves it out from under here on its own. What is left
/// is one process's own, split by lifetime so that each level's cleanup is one call: a
/// `stop` removes `machine/`, and the end of a session removes `{pid}/`.
///
/// A host with nowhere to keep this is a panic and not a temporary directory, unlike the
/// images: `session.ext4` is what a session wrote, and a server that put it somewhere the
/// machine may sweep would be losing it rather than saying it had nowhere to put it.
pub(crate) fn home() -> PathBuf {
    cortex_krun::home().expect("neither CORTEX_KRUN_HOME nor a cache directory is to be had")
}

#[tokio::main]
async fn main() -> ExitCode {
    match run(std::env::args().skip(1)).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // To stderr and never stdout: from the moment the server starts, stdout carries
            // frames and nothing else.
            eprintln!("{}: {e:#}", env!("CARGO_BIN_NAME"));
            ExitCode::from(NOT_EXECUTABLE)
        }
    }
}

async fn run(argv: impl IntoIterator<Item = impl AsRef<str>>) -> anyhow::Result<()> {
    match argv.into_iter().next().as_ref().map(AsRef::as_ref) {
        None | Some("run") => session::run().await,
        Some(other) => anyhow::bail!("{other:?} is not `run`, which is also what no argument is"),
    }
}
