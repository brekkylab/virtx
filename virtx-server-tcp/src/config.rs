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
//! `<home>/project/<id>/` (see [`virtx_uvm::ProjectStore`]). Its sandboxes are kept there across
//! the server's restart, each with a record beside its disk: parked as the server ends, and
//! known again, parked, as it starts (see [`virtx_uvm::Sandbox::kept`]).
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
//!
//! Its anonymous pulls of Docker Hub's images -- a pull, a build's base -- go to the mirror
//! `registry_mirror` names first, as a reference names its registry: `"registry_mirror":
//! "mirror.gcr.io"`. None, unless set. See [`virtx_uvm::RegistryClient::with_mirror`].
//!
//! `"share_public_images": true` shares the projects' anonymous pulls between them: a project's
//! pull of an image another pulled anonymously before links that one's layers, and pulls none
//! ([`crate::shared::SharedPulls`]). How long a pull takes then says whether another project
//! pulled the image, so it is off unless set: for a server whose projects' users may know that
//! of each other, such as one person's or one team's.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, anyhow};
use serde::Deserialize;
use virtx_uvm::{ProjectStore, PublicImages, PublicRegistries};

use crate::shared::SharedPulls;

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
    /// Docker Hub's mirror, which anonymous pulls of its images go to first: `host` or
    /// `host:port`. None, unless set.
    #[serde(default)]
    pub registry_mirror: Option<String>,
    /// Whether the projects' anonymous pulls are shared between them. Off unless set.
    #[serde(default)]
    pub share_public_images: bool,
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
        if let Some(mirror) = &config.registry_mirror {
            PublicRegistries::new([mirror]).map_err(|e| anyhow!("registry_mirror: {e}"))?;
        }
        Ok(config)
    }

    /// Where the projects' anonymous pulls are shared: between them, where
    /// `share_public_images` says so, and nowhere otherwise.
    pub fn public_images(&self) -> Option<Arc<dyn PublicImages>> {
        self.share_public_images.then(|| {
            Arc::new(SharedPulls::new(ProjectStore::new(self.home()))) as Arc<dyn PublicImages>
        })
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

    #[test]
    fn public_images_are_shared_between_projects_only_where_it_says_so() {
        let config = |share: Option<bool>| {
            let mut config = json!({
                "listen": "127.0.0.1:0",
                "oidc": { "issuer": "https://idp.example", "client_id": "virtx" },
            });
            if let Some(share) = share {
                config["share_public_images"] = json!(share);
            }
            Config::parse(config).unwrap()
        };
        assert!(config(None).public_images().is_none());
        assert!(config(Some(false)).public_images().is_none());
        assert!(config(Some(true)).public_images().is_some());
    }

    #[test]
    fn a_registry_mirror_is_named_by_its_host_or_its_host_and_port() {
        let config = |mirror: &str| {
            Config::parse(json!({
                "listen": "127.0.0.1:0",
                "oidc": { "issuer": "https://idp.example", "client_id": "virtx" },
                "registry_mirror": mirror,
            }))
        };
        let read = config("mirror.gcr.io").unwrap();
        assert_eq!(read.registry_mirror.as_deref(), Some("mirror.gcr.io"));
        assert!(config("registry.internal:5000").is_ok());
        let refused = config("https://mirror.gcr.io").err().unwrap();
        assert_eq!(
            refused.to_string(),
            "registry_mirror: \"https://mirror.gcr.io\": \
             a registry is named `host` or `host:port`"
        );
    }
}
