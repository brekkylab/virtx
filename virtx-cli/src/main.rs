//! `virtx`: build, list, inspect, tag, remove, pull and push a virtx server's images from a
//! shell.
//!
//! The spelling follows `docker`'s, in both of its forms: `virtx image <command>`, and the
//! top-level `build`, `images`, `inspect`, `rmi`, `pull`, `push` and `tag`, which are the same
//! commands.
//! `command.rs` is the command tree and how it runs, `server.rs` how the server is reached,
//! `job.rs` how a job's log is followed, and `name.rs` how an argument names an image; each
//! command's arguments and handler are in the file of its name.

mod build;
mod command;
mod inspect;
mod job;
mod ls;
mod name;
mod prompt;
mod pull;
mod push;
mod registry;
mod rm;
mod server;
mod tag;

use std::process::ExitCode;

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    ExitCode::from(command::run(std::env::args_os()).await)
}
