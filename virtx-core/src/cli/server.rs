//! Reaching the server: one at a URL over HTTP, or `virtx-stdio` started here, over its stdin and
//! stdout -- the one of this version, fetched if this host has none, or the one named.

use std::{io, path::PathBuf};

use anyhow::Context as _;
use clap::Args;
use tokio::process::Command;

use crate::{Auth, Client, ensure_virtx};

/// The bearer token a server at `--host` wants, taken from the environment alone so it is never
/// on a command line.
const TOKEN: &str = "VIRTX_TOKEN";

#[derive(Debug, PartialEq, Args)]
pub(crate) struct ServerArgs {
    /// The server's base URL, reached over HTTP with `$VIRTX_TOKEN` as its bearer token if set;
    /// a server started here if absent.
    #[arg(
        short = 'H',
        long,
        env = "VIRTX_HOST",
        global = true,
        value_name = "URL"
    )]
    pub(crate) host: Option<String>,

    /// The server to start; if absent, `virtx-stdio` of this version, fetched into the cache the
    /// first time.
    #[arg(
        long,
        env = "VIRTX_STDIO",
        global = true,
        conflicts_with = "host",
        value_name = "PROGRAM"
    )]
    pub(crate) server: Option<PathBuf>,
}

/// A client of the server `args` names.
pub(crate) async fn connect(args: &ServerArgs) -> anyhow::Result<Client> {
    if let Some(url) = &args.host {
        let auth = match std::env::var(TOKEN) {
            Ok(token) if !token.is_empty() => Auth::Bearer { token },
            _ => Auth::None,
        };
        return Client::connect(url, auth, http::HeaderMap::new())
            .with_context(|| format!("reaching {url}"));
    }
    let program = match &args.server {
        Some(program) => program.clone(),
        None => ensure_virtx().await.context("fetching the server")?,
    };
    Client::spawn(Command::new(&program)).map_err(|error| {
        let started = format!("starting {}", program.display());
        if error.kind() == io::ErrorKind::NotFound {
            anyhow::Error::new(error).context(format!("{started}: no such program"))
        } else {
            anyhow::Error::new(error).context(started)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_server_named_but_missing_says_so() {
        let args = ServerArgs {
            host: None,
            server: Some("/nonexistent/virtx-stdio".into()),
        };
        let error = connect(&args).await.err().unwrap();
        assert!(
            format!("{error:#}").contains("/nonexistent/virtx-stdio: no such program"),
            "{error:#}"
        );
    }
}
