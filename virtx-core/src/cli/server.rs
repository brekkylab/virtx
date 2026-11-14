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

/// The server's base URL when neither `--host` nor `--server` is given.
const HOST: &str = "VIRTX_HOST";

#[derive(Debug, PartialEq, Args)]
pub(crate) struct ServerArgs {
    /// The server's base URL, reached over HTTP with `$VIRTX_TOKEN` as its bearer token if set;
    /// if absent, `$VIRTX_HOST`, unless `--server` names a server to start.
    #[arg(short = 'H', long, global = true, value_name = "URL")]
    pub(crate) host: Option<String>,

    /// The server to start; if absent, the one `$VIRTX_STDIO` names, or else `virtx-stdio` of
    /// this version, fetched into the cache the first time. A host, from `--host` or
    /// `$VIRTX_HOST`, is reached in place of `$VIRTX_STDIO`'s.
    #[arg(long, global = true, conflicts_with = "host", value_name = "PROGRAM")]
    pub(crate) server: Option<PathBuf>,
}

/// The base URL of the server to reach over HTTP, if `args` and the environment name one:
/// `--host`, or `$VIRTX_HOST` when `--server` names no server to start.
pub(crate) fn host(args: &ServerArgs) -> Option<String> {
    if args.host.is_some() || args.server.is_some() {
        return args.host.clone();
    }
    std::env::var(HOST).ok().filter(|url| !url.is_empty())
}

/// A client of the server `args` names.
pub(crate) async fn connect(args: &ServerArgs) -> anyhow::Result<Client> {
    if let Some(url) = host(args) {
        let auth = match std::env::var(TOKEN) {
            Ok(token) if !token.is_empty() => Auth::Bearer { token },
            _ => Auth::None,
        };
        return Client::connect(&url, auth, http::HeaderMap::new())
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
