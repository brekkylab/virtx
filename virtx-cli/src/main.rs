//! `virtx`: build, list, tag, move and remove a virtx server's images from a shell.
//!
//! The spelling follows `docker`'s, in both of its forms: `virtx image <command>`, and the
//! top-level `build`, `images`, `inspect`, `rmi`, `pull`, `push`, `tag`, `save` and `load`,
//! which are the same commands; `prune` is `image prune` alone, as in `docker`.
//!
//! `command.rs` is the command tree and how it runs, `server.rs` how the server is reached,
//! `job.rs` and `transfer.rs` how a job or a transfer is followed, and `name.rs` how an argument
//! names an image; each command's arguments and handler are in the file of its name.

mod build;
mod command;
mod inspect;
mod job;
mod load;
mod ls;
mod name;
mod prompt;
mod prune;
mod pull;
mod push;
mod registry;
mod rm;
mod save;
mod server;
mod tag;
mod transfer;

use std::process::ExitCode;

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    ExitCode::from(command::run(std::env::args_os()).await)
}
