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
//! A project's snapshots, volumes and images are under `<home>/project/<name>/`, its own.
//! Without `home`, it is [`virtx_uvm::cache_root`].
//!
//! With `oidc`, a request's token is an ID token its issuer signed for `client_id`, and the
//! user is its `username_claim` (`email` unless given): a project's members are users of the
//! provider's.
//!
//! Without `oidc` the server authenticates no one, every request let into every project, and
//! into `default` when it names none -- the one project there is when the config names none
//! either. So only on the loopback: listening elsewhere, it is refused unless it says
//! `"auth": "none"`.

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
    #[serde(default)]
    pub oidc: Option<crate::oidc::Config>,
    #[serde(default)]
    pub(crate) auth: Option<Auth>,
    #[serde(default)]
    pub projects: HashMap<String, Project>,
}

/// What the config says of authentication, beyond naming a provider.
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Auth {
    None,
}

/// The project a request naming none reaches, on a server without authentication.
pub const DEFAULT_PROJECT: &str = "default";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Project {
    #[serde(default)]
    pub members: Vec<String>,
}

impl Config {
    /// The config at `path`.
    pub fn read(path: &Path) -> anyhow::Result<Config> {
        let text = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let value = serde_json::from_slice(&text);
        let value = value.with_context(|| format!("reading {}", path.display()))?;
        Config::parse(value).with_context(|| format!("reading {}", path.display()))
    }

    /// The config `value` is, refused when it names a project by a name that is no
    /// directory's.
    pub fn parse(value: serde_json::Value) -> anyhow::Result<Config> {
        let config: Config = serde_json::from_value(value)?;
        if let Some(name) = config
            .projects
            .keys()
            .find(|name| !virtx_uvm::is_name(name))
        {
            bail!("project {name:?}: a name is letters, digits, '.', '_' and '-'");
        }
        Ok(config)
    }

    /// Where everything is kept.
    pub fn home(&self) -> PathBuf {
        self.home.clone().unwrap_or_else(virtx_uvm::cache_root)
    }
}
