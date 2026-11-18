//! The config file: where to listen, where projects are kept, and whose tokens a request carries.
//!
//! ```json
//! {
//!   "listen": "127.0.0.1:8470",
//!   "home": "/var/lib/virtx",
//!   "origins": [],
//!   "oidc": { "issuer": "https://idp.example", "client_id": "virtx", "username_claim": "email" }
//! }
//! ```
//!
//! A project's volumes, images and its sandboxes' upper disks are under its own root,
//! `<home>/project/<id>/` (see [`virtx_uvm::ProjectStore`]).
//! Without `home`, it is [`virtx_uvm::cache_root`].
//!
//! A request's token is a token `oidc`'s issuer signed for `client_id`, and the user is its
//! `username_claim` (`email` unless given). The issuer's keys are found through its OpenID
//! Connect discovery document, or, for an issuer that has none, at `oidc`'s `jwks_uri`. A
//! project's members are users of the issuer's, kept in its root and changed at `/projects/`,
//! where anyone signed in makes a project and owns it. A request that names no project is in its
//! sender's own, which the server makes the first time. Without authentication, a client reaches
//! its projects through `virtx-stdio` instead.
//!
//! An image job -- a pull, a push, a build's base -- connects to registries on public
//! addresses, and to one on this host or a private network only if `private_registries` names
//! it, as a reference names its registry: `"private_registries": ["registry.internal:5000"]`.
//! See [`virtx_uvm::PublicRegistries`].

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, anyhow};
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
    pub oidc: crate::oidc::Config,
    /// The registries image jobs reach on this host or a private network: each `host`, at any
    /// port, or `host:port`. A host a registry sends a job on to, for a token or a layer, is
    /// reached on a private address only if it is listed too.
    #[serde(default)]
    pub private_registries: Vec<String>,
}

impl Config {
    /// The config at `path`.
    pub fn read(path: &Path) -> anyhow::Result<Config> {
        let read = || Config::parse(serde_json::from_slice(&std::fs::read(path)?)?);
        read().with_context(|| format!("reading {}", path.display()))
    }

    /// The config `value` is, refused when it names a registry by what is not a host or a host
    /// and its port.
    pub fn parse(value: serde_json::Value) -> anyhow::Result<Config> {
        let config: Config = serde_json::from_value(value)?;
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
            Config::parse(json!({
                "listen": "127.0.0.1:0",
                "oidc": { "issuer": "https://idp.example", "client_id": "virtx" },
                "private_registries": registries,
            }))
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
