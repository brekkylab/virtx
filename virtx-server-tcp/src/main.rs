//! `virtx-server-tcp` -- a virtx server over HTTP, to the projects its config names and the users
//! its OpenID Connect provider does (see [`virtx_server_tcp::config`]): each request carries a
//! user's token and names a project, and the endpoints the protocol names answer it from that
//! project's home; see [`virtx_server_tcp::serve`] for the rest.
//!
//! `virtx-server-tcp vm …` is this binary spawned by itself to be a machine; see
//! [`virtx_uvm::Machine`].

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use virtx_server_tcp::config::Config;
use virtx_server_tcp::serve;

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
    serve::keep_pruned(Arc::clone(&access));
    let listener = serve::bind(config.listen).await?;
    if let Some(warning) = plain_http_warning(listener.local_addr()?) {
        eprintln!("{warning}");
    }
    let served = serve::run(listener, router).await;
    // Each sandbox parked and kept, for this server started again to know it.
    tokio::task::spawn_blocking(move || {
        for home in access.homes() {
            home.leave_all();
        }
    })
    .await?;
    served
}

/// What this server says on its standard error, listening at `at`, where that is not this
/// machine's loopback: that its HTTP is plain and wants HTTPS in front. The binary says it and
/// [`serve::bind`] does not, because a server built on the library may sit behind TLS that
/// [`serve::bind`] cannot see.
fn plain_http_warning(at: std::net::SocketAddr) -> Option<String> {
    (!at.ip().to_canonical().is_loopback()).then(|| {
        format!(
            "{at} is not this machine's loopback, and the server speaks plain HTTP: serve it \
             to other machines over HTTPS from a proxy in front, or their tokens and files \
             cross the network in the clear"
        )
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_server_off_the_loopback_says_its_http_is_plain() {
        let said = |at: &str| super::plain_http_warning(at.parse().unwrap());
        for at in ["127.0.0.1:8470", "[::1]:8470", "[::ffff:127.0.0.1]:8470"] {
            assert_eq!(said(at), None, "{at}");
        }
        for at in ["0.0.0.0:8470", "192.0.2.1:8470", "[::]:8470"] {
            let warning = said(at).unwrap();
            assert!(warning.starts_with(at), "{warning}");
            assert!(warning.contains("plain HTTP"), "{warning}");
        }
    }
}
