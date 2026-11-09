//! `virtx-server-tcp` -- a virtx server over HTTP, to the users and projects its config names
//! (see [`config`]): each request carries a user's token and names a project, and the endpoints
//! `virtx-core`'s `PROTOCOL.md` names answer it from that project's home; see [`serve`] for the
//! rest.
//!
//! `virtx-server-tcp token` makes a token, and says what the config keeps of it.
//! `virtx-server-tcp vm …` is this binary spawned by itself to be a machine; see
//! [`virtx_uvm::Machine`].

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use virtx_server_tcp::access::Access;
use virtx_server_tcp::config::{self, Config};
use virtx_server_tcp::{endpoints, serve};

#[derive(Parser)]
enum Command {
    /// Serve the endpoints, and sandboxes' ports beside them, as `--config` says.
    Serve {
        #[arg(long)]
        config: PathBuf,
    },
    /// Make a token for a user, and its `token_sha256` for the config.
    Token,
}

fn main() -> anyhow::Result<()> {
    // Before any runtime: this process may be about to become a machine and never come back.
    serve::become_machine_if_spawned();
    match Command::parse() {
        Command::Serve { config } => run(Config::read(&config)?),
        Command::Token => {
            let token = format!(
                "{}{}",
                uuid::Uuid::new_v4().simple(),
                uuid::Uuid::new_v4().simple()
            );
            println!("token:        {token}");
            println!("token_sha256: {}", config::hashed(&token));
            Ok(())
        }
    }
}

#[tokio::main]
async fn run(config: Config) -> anyhow::Result<()> {
    if let Some(secs) = config.minute_secs {
        endpoints::set_minute(std::time::Duration::from_secs(secs.max(1)));
    }
    if let Some(capacity) = config.capacity {
        endpoints::set_capacity(endpoints::Capacity {
            vcpus: capacity.vcpus,
            memory_mib: capacity.memory_mib,
        });
    }
    let access = Arc::new(Access::new(&config)?);
    let router = serve::router(Arc::clone(&access), &config.origins, axum::Router::new());
    serve::spawn_loops(&access);
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
