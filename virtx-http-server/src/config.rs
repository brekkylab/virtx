//! The config file: where to listen, where projects are kept, and who may reach which project.
//!
//! ```json
//! {
//!   "listen": "127.0.0.1:8470",
//!   "home": "/var/lib/virtx",
//!   "origins": [],
//!   "users": { "alice": { "token_sha256": "<64 hex digits>" } },
//!   "projects": { "demo": { "members": ["alice"] } }
//! }
//! ```
//!
//! A project's snapshots and volumes are under `<home>/project/<name>/`; the images are under
//! `<home>/images/`, shared by every project. Without `home`, it is
//! [`virtx_uvm::cache_root`], and the images are where the stdio server keeps them too.
//!
//! A token is kept as its SHA-256, so the file does not hold what it would take to use one.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};
use serde::Deserialize;
use sha2::{Digest as _, Sha256};

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
    users: HashMap<String, User>,
    pub projects: HashMap<String, Project>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct User {
    token_sha256: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Project {
    pub members: Vec<String>,
}

impl Config {
    /// The config at `path`, refused when it names a member nobody is, a token twice, or a
    /// project by a name that is no directory's.
    pub fn read(path: &Path) -> anyhow::Result<Config> {
        let text = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let config: Config =
            serde_json::from_slice(&text).with_context(|| format!("reading {}", path.display()))?;
        let mut tokens = HashMap::new();
        for (name, user) in &config.users {
            let hash = &user.token_sha256;
            if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
                bail!("user {name}: token_sha256 is not 64 hex digits");
            }
            if let Some(other) = tokens.insert(hash.to_ascii_lowercase(), name) {
                bail!("users {other} and {name} have the same token");
            }
        }
        for (name, project) in &config.projects {
            if !virtx_uvm::is_name(name) {
                bail!("project {name:?}: a name is letters, digits, '.', '_' and '-'");
            }
            if let Some(nobody) = project
                .members
                .iter()
                .find(|member| !config.users.contains_key(*member))
            {
                bail!("project {name}: member {nobody} is not a user");
            }
        }
        Ok(config)
    }

    /// Where everything is kept.
    pub fn home(&self) -> PathBuf {
        self.home.clone().unwrap_or_else(virtx_uvm::cache_root)
    }

    /// Each user, by the SHA-256 of their token in lower-case hex.
    pub fn users(&self) -> HashMap<String, String> {
        self.users
            .iter()
            .map(|(name, user)| (user.token_sha256.to_ascii_lowercase(), name.clone()))
            .collect()
    }
}

/// `token`'s SHA-256, in lower-case hex: what the config keeps of it.
pub fn hashed(token: &str) -> String {
    format!("{:x}", Sha256::digest(token.as_bytes()))
}
