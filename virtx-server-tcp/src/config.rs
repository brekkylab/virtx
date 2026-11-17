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
//! A project's snapshots, volumes and images are under its own root, `<home>/project/<id>/`
//! (see [`virtx_uvm::ProjectStore`]). Without `home`, it is [`virtx_uvm::cache_root`].
//!
//! A request's token is a token `oidc`'s issuer signed for `client_id`, and the user is its
//! `username_claim` (`email` unless given). The issuer's keys are found through its OpenID
//! Connect discovery document, or, for an issuer that has none, at `oidc`'s `jwks_uri`. A
//! project's members are users of the issuer's, kept in its root and changed at `/projects/`,
//! where anyone signed in makes a project and owns it. A request that names no project is in its
//! sender's own, which the server makes the first time. Without authentication, a client reaches
//! its projects through `virtx-stdio` instead.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
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
}

impl Config {
    /// The config at `path`.
    pub fn read(path: &Path) -> anyhow::Result<Config> {
        let read = || Config::parse(serde_json::from_slice(&std::fs::read(path)?)?);
        read().with_context(|| format!("reading {}", path.display()))
    }

    /// The config `value` is.
    pub fn parse(value: serde_json::Value) -> anyhow::Result<Config> {
        Ok(serde_json::from_value(value)?)
    }

    /// Where everything is kept.
    pub fn home(&self) -> PathBuf {
        self.home.clone().unwrap_or_else(virtx_uvm::cache_root)
    }
}
