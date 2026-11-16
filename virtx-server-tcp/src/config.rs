//! The config file: where to listen, where projects are kept, and who may reach which project.
//!
//! ```json
//! {
//!   "listen": "127.0.0.1:8470",
//!   "home": "/var/lib/virtx",
//!   "origins": [],
//!   "oidc": { "issuer": "https://idp.example", "client_id": "virtx", "username_claim": "email" },
//!   "projects": { "demo": { "members": ["alice@example.com"] } }
//! }
//! ```
//!
//! A project's snapshots, volumes and images are under its own root, `<home>/project/<name>/`
//! (see [`virtx_uvm::ProjectStore`]). Without `home`, it is [`virtx_uvm::cache_root`].
//!
//! A request's token is an ID token `oidc`'s issuer signed for `client_id`, and the user is its
//! `username_claim` (`email` unless given): a project's members are users of the provider's,
//! kept beside its root and changed at `/projects/`, where anyone signed in makes a project and
//! owns it. `projects` are made as the server starts, each with its members its owners, where a
//! project of the name is not there already: from then on its members are its own, and the
//! config's are not read again.
//!
//! A request that names no project is in `default`, a project as any other, there once it is
//! made. Without authentication, a client reaches its projects through `virtx-stdio` instead.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub listen: SocketAddr,
    #[serde(default)]
    home: Option<PathBuf>,
    /// The `Origin`s a browser may call from: each is answered its preflights, and let
    /// through with the token and the project as any other caller.
    #[serde(default)]
    pub origins: Vec<String>,
    pub oidc: crate::oidc::Config,
    #[serde(default)]
    pub projects: HashMap<String, Project>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Project {
    #[serde(default)]
    pub members: Vec<String>,
}

impl Config {
    /// The config at `path`.
    pub fn read(path: &Path) -> anyhow::Result<Config> {
        let read = || Config::parse(serde_json::from_slice(&std::fs::read(path)?)?);
        read().with_context(|| format!("reading {}", path.display()))
    }

    /// The config `value` is, refused when it names a project by a name that
    /// [is no project's](virtx_uvm::ProjectStore::is_name).
    pub fn parse(value: serde_json::Value) -> anyhow::Result<Config> {
        let config: Config = serde_json::from_value(value)?;
        if let Some(name) = config
            .projects
            .keys()
            .find(|name| !virtx_uvm::ProjectStore::is_name(name))
        {
            bail!("project {name:?}: a name is lower-case letters, digits, '.', '_' and '-'");
        }
        Ok(config)
    }

    /// Where everything is kept.
    pub fn home(&self) -> PathBuf {
        self.home.clone().unwrap_or_else(virtx_uvm::cache_root)
    }
}
