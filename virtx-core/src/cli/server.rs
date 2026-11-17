//! Reaching the server: one at a URL over HTTP, or `virtx-stdio` started here, over its stdin and
//! stdout -- the one of this version, fetched if this host has none, or the one named.

use std::{io, path::PathBuf};

use anyhow::Context as _;
use clap::Args;
use tokio::process::Command;

use crate::{Auth, Client, PROJECT_HEADER, ensure_virtx};

/// The bearer token the server reached over HTTP wants, taken from the environment alone so it
/// is never on a command line.
const TOKEN: &str = "VIRTX_TOKEN";

/// The server's base URL when neither `--host` nor `--server` is given.
const HOST: &str = "VIRTX_HOST";

/// The project when `--project` is not given.
const PROJECT: &str = "VIRTX_PROJECT";

#[derive(Debug, PartialEq, Args)]
pub(crate) struct ServerArgs {
    /// The server's base URL, reached over HTTP with `$VIRTX_TOKEN` as its bearer token if set;
    /// if absent, `$VIRTX_HOST`, unless `--server` names a server to start.
    #[arg(short = 'H', long, global = true, value_name = "URL")]
    pub(crate) host: Option<String>,

    /// The project the requests belong to -- a project of the user's by its name, or any by its
    /// id -- sent as the `virtx-project` header; if absent, `$VIRTX_PROJECT`, and without
    /// either, the user's own.
    #[arg(long, global = true, value_name = "NAME")]
    pub(crate) project: Option<String>,

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

/// The project the requests belong to, if `args` and the environment name one: `--project`, or
/// else `$VIRTX_PROJECT`.
pub(crate) fn project(args: &ServerArgs) -> Option<String> {
    args.project
        .clone()
        .or_else(|| std::env::var(PROJECT).ok().filter(|name| !name.is_empty()))
}

/// A client of the server `args` names, in the project they name.
pub(crate) async fn connect(args: &ServerArgs) -> anyhow::Result<Client> {
    let client = match host(args) {
        Some(url) => {
            let auth = match std::env::var(TOKEN) {
                Ok(token) if !token.is_empty() => Auth::Bearer { token },
                _ => Auth::None,
            };
            Client::connect(&url, auth, http::HeaderMap::new())
                .with_context(|| format!("reaching {url}"))?
        }
        None => {
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
            })?
        }
    };
    if let Some(project) = project(args) {
        client.set_project(Some(&project)).with_context(|| {
            format!("the project {project:?} cannot be sent in the {PROJECT_HEADER} header")
        })?;
    }
    Ok(client)
}

#[cfg(test)]
mod tests {
    use axum::{Json, Router, routing::get};
    use tokio::net::TcpListener;

    use super::*;
    use crate::transport::http;

    #[tokio::test]
    async fn a_host_is_sent_the_project_in_the_virtx_project_header() {
        // `/project` answers the project its request names, and `400` for none, as
        // `virtx-server-tcp` refuses one.
        let named = async |headers: ::http::HeaderMap| {
            let project = headers.get(PROJECT_HEADER).and_then(|v| v.to_str().ok());
            project
                .map(|name| Json(name.to_owned()))
                .ok_or(::http::StatusCode::BAD_REQUEST)
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let router = Router::new().route("/project", get(named));
        tokio::spawn(async move { axum::serve(listener, router).await });

        let args = ServerArgs {
            host: Some(url),
            project: Some("demo".into()),
            server: None,
        };
        let client = connect(&args).await.unwrap();
        let sent = http::get::<String>(&client.inner, "/project").await;
        assert_eq!(sent.unwrap(), "demo");

        let args = ServerArgs {
            project: Some("a\nb".into()),
            ..args
        };
        let error = connect(&args).await.err().unwrap();
        assert!(
            format!("{error:#}").contains(r#"the project "a\nb" cannot be sent"#),
            "{error:#}"
        );
    }

    #[tokio::test]
    async fn a_server_named_but_missing_says_so() {
        let args = ServerArgs {
            host: None,
            project: None,
            server: Some("/nonexistent/virtx-stdio".into()),
        };
        let error = connect(&args).await.err().unwrap();
        assert!(
            format!("{error:#}").contains("/nonexistent/virtx-stdio: no such program"),
            "{error:#}"
        );
    }
}
