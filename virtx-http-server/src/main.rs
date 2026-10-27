//! `virtx-http-server` -- a virtx server over HTTP, to the users and projects its config names
//! (see [`config`]): each request carries a user's token and names a project, and its methods
//! are in that project's home.
//!
//! `virtx-http-server token` makes a token, and says what the config keeps of it.
//! `virtx-http-server vm …` is this binary spawned by itself to be a machine; see
//! [`virtx_uvm::Machine`].

mod access;
mod config;
mod rpc;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use axum::response::IntoResponse as _;
use clap::Parser;
use virtx_core::{Dispatcher, Server};

use crate::access::Access;
use crate::config::Config;

/// How long a sandbox is kept with no command run in it: over HTTP no client is ever known to
/// be gone, so one that stops naming its sandboxes is taken to be, and what they hold -- their
/// snapshots -- let go.
const IDLE: Duration = Duration::from_secs(15 * 60);

#[derive(Parser)]
enum Command {
    /// Serve at the endpoint `/rpc`, as `--config` says.
    Serve {
        #[arg(long)]
        config: PathBuf,
    },
    /// Make a token for a user, and its `token_sha256` for the config.
    Token,
}

fn main() -> anyhow::Result<()> {
    // Before any runtime: this process is about to become a machine and never come back.
    if std::env::args_os()
        .nth(1)
        .is_some_and(|arg| arg == virtx_uvm::MACHINE_SUBCOMMAND)
    {
        // Why it could not be is the host's already, told on the control channel; stderr is
        // the guest command's, and nothing of this process's goes there.
        let Err(_) = virtx_uvm::enter_machine();
        std::process::exit(1);
    }
    match Command::parse() {
        Command::Serve { config } => serve(Config::read(&config)?),
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
async fn serve(config: Config) -> anyhow::Result<()> {
    let access = Arc::new(Access::new(&config)?);
    let server = Server::new(Dispatcher::new(rpc::methods()));
    let checked = Arc::clone(&access);
    let router = axum::Router::new().route(
        "/rpc",
        axum::routing::any(move |mut request: axum::extract::Request| {
            let server = server.clone();
            let home = checked.home(request.headers());
            async move {
                match home {
                    Ok(home) => {
                        request.extensions_mut().insert(home);
                        server.handle(request).await.into_response()
                    }
                    Err(status) => status.into_response(),
                }
            }
        }),
    );

    let reaping = Arc::clone(&access);
    tokio::spawn(async move {
        let mut every = tokio::time::interval(Duration::from_secs(60));
        loop {
            every.tick().await;
            for home in reaping.homes() {
                home.forget_idle(IDLE);
            }
        }
    });

    let listener = tokio::net::TcpListener::bind(config.listen)
        .await
        .with_context(|| format!("listening at {}", config.listen))?;
    eprintln!("serving at http://{}/rpc", listener.local_addr()?);
    // A command still running at the end ends with this process, whose machines are killed as
    // their calls drop.
    axum::serve(listener, router)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
