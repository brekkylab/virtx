mod boot;
mod image;
mod image_build;
mod session;

use std::process::ExitCode;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// What to do; nothing at all is `session`, because a client spawns this binary to be its
    /// server and passes nothing -- the common case is the one a caller does not spell.
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Manage the images sessions boot from.
    #[command(subcommand)]
    Image(image::Command),
    /// Answer a console session on stdin and stdout (the default).
    Session,
    /// Become a micro-VM (internal).
    #[command(hide = true)]
    Boot(boot::Args),
}

fn main() -> ExitCode {
    let result = match Cli::parse().command.unwrap_or(Command::Session) {
        // Before any runtime is made: this process is about to become a machine and never
        // come back, and has no use for a pool of threads it would carry into the VMM.
        Command::Boot(args) => boot::enter(args).map(|never| match never {}),
        Command::Image(command) => runtime().and_then(|rt| rt.block_on(image::run(command))),
        Command::Session => runtime().and_then(|rt| rt.block_on(session::run())),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{}: {e:#}", env!("CARGO_BIN_NAME"));
            ExitCode::FAILURE
        }
    }
}

fn runtime() -> anyhow::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| anyhow::anyhow!("starting the async runtime: {e}"))
}
