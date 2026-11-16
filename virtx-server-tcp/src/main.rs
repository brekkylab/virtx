//! `virtx-server-tcp` -- a virtx server over HTTP, to the projects its config names and the
//! users its OpenID Connect provider does (see [`virtx_server_tcp::config`]): each request
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
    if let Some(secs) = config.minute_secs {
        endpoints::set_minute(std::time::Duration::from_secs(secs.max(1)));
    }
    if let Some(mirror) = config
        .registry_mirror
        .as_deref()
        .filter(|m| !m.trim().is_empty())
    {
        endpoints::set_registry_mirror(mirror);
    }
    let (projects, access, router) = serve::configured(&config)?;
    serve::spawn_loops(&access);
    if projects.open() {
        eprintln!("without authentication: every request reaches every project");
    }
    let served = serve::run(config.listen, router).await;
    // Every sandbox stops with this server, what it wrote on its disk, kept for the next.
    tokio::task::spawn_blocking(move || {
        for home in access.homes() {
            home.close_all_blocking();
        }
    })
    .await?;
    served
}
