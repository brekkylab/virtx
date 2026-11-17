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
//! A project's volumes, images and its sandboxes' upper disks are under
//! `<home>/project/<name>/`, its own.
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
//!
//! An image job -- a pull, a push, a build's base -- connects to registries on public
//! addresses, and to one on this host or a private network only if `private_registries` names
//! it, as a reference names its registry: `"private_registries": ["registry.internal:5000"]`.
//! See [`virtx_uvm::PublicRegistries`].

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, anyhow, bail};
use serde::Deserialize;
use virtx_uvm::PublicRegistries;

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
    /// The registries image jobs reach on this host or a private network: each `host`, at any
    /// port, or `host:port`.
    #[serde(default)]
    pub private_registries: Vec<String>,
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
        let read = || Config::parse(serde_json::from_slice(&std::fs::read(path)?)?);
        read().with_context(|| format!("reading {}", path.display()))
    }

    /// The config `value` is, refused when it names a project by a name that is no
    /// directory's, or a registry by what is not a host or a host and its port.
    pub fn parse(value: serde_json::Value) -> anyhow::Result<Config> {
        let config: Config = serde_json::from_value(value)?;
        if let Some(name) = config
            .projects
            .keys()
            .find(|name| !virtx_uvm::is_name(name))
        {
            bail!("project {name:?}: a name is letters, digits, '.', '_' and '-'");
        }
        config.registries()?;
        Ok(config)
    }

    /// The proxy this server's image jobs connect through.
    pub fn registries(&self) -> anyhow::Result<PublicRegistries> {
        PublicRegistries::new(&self.private_registries)
            .map_err(|e| anyhow!("private_registries: {e}"))
    }

    /// Where everything is kept.
    pub fn home(&self) -> PathBuf {
        self.home.clone().unwrap_or_else(virtx_uvm::cache_root)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::Config;

    #[test]
    fn a_private_registry_is_named_by_its_host_or_its_host_and_port() {
        let config = |registries: serde_json::Value| {
            Config::parse(json!({ "listen": "127.0.0.1:0", "private_registries": registries }))
        };
        assert!(config(json!(["registry.internal:5000", "10.0.0.5"])).is_ok());
        let refused = config(json!(["https://registry.internal"])).err().unwrap();
        assert_eq!(
            refused.to_string(),
            "private_registries: \"https://registry.internal\": \
             a registry is named `host` or `host:port`"
        );
    }
}
