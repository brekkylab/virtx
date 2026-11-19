//! `virtx-server-tcp` -- a virtx server over HTTP, to the users its OpenID Connect provider
//! names (see [`virtx_server_tcp::config`]): each request
//! carries a user's token and names a project, and the endpoints `virtx-core`'s `PROTOCOL.md`
//! names answer it from that project's home; see [`virtx_server_tcp::serve`] for the rest.
//!
//! `virtx-server-tcp vm …` is this binary spawned by itself to be a machine; see
//! [`virtx_uvm::Machine`].

use std::path::PathBuf;

use clap::Parser;
use virtx_server_tcp::config::Config;
use virtx_server_tcp::{endpoints, serve};

#[derive(Parser)]
enum Command {
    /// Serve the endpoints, and sandboxes' ports beside them, as `--config` says.
    Serve {
        #[arg(long)]
        config: PathBuf,
    },
}

fn main() -> anyhow::Result<()> {
    // Before any runtime: this process may be about to become a machine and never come back.
    serve::become_machine_if_spawned();
    match Command::parse() {
        Command::Serve { config } => run(Config::read(&config)?),
    }
}

#[tokio::main]
async fn run(config: Config) -> anyhow::Result<()> {
    let (access, router) = serve::configured(&config).await?;
    serve::spawn_loops(&access);
    let listener = serve::bind(config.listen).await?;
    let served = serve::serve(listener, router).await;
    // Every sandbox is parked with this server, what it wrote on its disk, kept for the next.
    tokio::task::spawn_blocking(move || {
        for home in access.homes() {
            home.close_all_blocking();
        }
    })
    .await?;
    served
}
