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
//! A project's snapshots and volumes are under `<home>/project/<name>/`; the images are under
//! `<home>/images/`, shared by every project. Without `home`, it is
//! [`virtx_uvm::cache_root`], and the images are where the stdio server keeps them too.
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
    auth: Option<Auth>,
    #[serde(default)]
    pub projects: HashMap<String, Project>,
    /// Whether requests are let through without credentials.
    #[serde(skip)]
    pub open: bool,
}

/// What the config says of authentication, beyond naming a provider.
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Auth {
    None,
}

/// The project a request naming none reaches, on a server without authentication.
pub const DEFAULT_PROJECT: &str = "default";

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Project {
    #[serde(default)]
    pub members: Vec<String>,
}

impl Config {
    /// The config at `path`, refused when it names a project by a name that is no
    /// directory's, or no provider while listening off the loopback without saying so.
    pub fn read(path: &Path) -> anyhow::Result<Config> {
        let text = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let mut config: Config =
            serde_json::from_slice(&text).with_context(|| format!("reading {}", path.display()))?;
        config.open = match (&config.auth, &config.oidc) {
            (Some(Auth::None), Some(_)) => bail!("auth is none, with oidc to authenticate"),
            (None, None) if !config.listen.ip().is_loopback() => bail!(
                "listening at {} for other machines with no oidc: \
                 say `\"auth\": \"none\"` to serve them without authentication",
                config.listen
            ),
            (_, oidc) => oidc.is_none(),
        };
        if config.open && config.projects.is_empty() {
            let project = Project::default();
            config.projects.insert(DEFAULT_PROJECT.to_owned(), project);
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn read(name: &str, config: serde_json::Value) -> anyhow::Result<Config> {
        let path = std::env::temp_dir().join(format!("virtx-config-{name}-{}", std::process::id()));
        std::fs::write(&path, config.to_string()).unwrap();
        let config = Config::read(&path);
        let _ = std::fs::remove_file(&path);
        config
    }

    #[test]
    fn without_a_provider_it_is_open_on_the_loopback_alone_unless_it_says_so() {
        use serde_json::json;
        let local = read("local", json!({ "listen": "127.0.0.1:0" })).unwrap();
        assert!(local.open);
        assert!(local.projects.contains_key(DEFAULT_PROJECT));

        let refused = read("wide", json!({ "listen": "0.0.0.0:0" }))
            .err()
            .unwrap();
        assert!(refused.to_string().contains("auth"), "{refused}");
        let said = read("said", json!({ "listen": "0.0.0.0:0", "auth": "none" })).unwrap();
        assert!(said.open);

        let oidc = json!({ "issuer": "https://idp.example", "client_id": "virtx" });
        let checked = read("checked", json!({ "listen": "0.0.0.0:0", "oidc": oidc })).unwrap();
        assert!(!checked.open);
        assert!(checked.projects.is_empty());
        let both = json!({ "listen": "127.0.0.1:0", "oidc": oidc, "auth": "none" });
        assert!(read("both", both).is_err());
    }
}
