//! Who may send a request, and which project's [`Home`] it reaches. [`Access`] lets a request
//! in from where a browser may call, and asks its [`Tenants`] the rest: whose the token is,
//! and the home of the project it names, whose images are its own.
//!
//! The server's own are [`Projects`]: the projects its config names, each to its members
//! among the users of the config's OpenID Connect provider ([`Oidc`]) -- or, of a config that
//! names no provider, to [`ANYONE`]. A server built on this one gives tenants of its own.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::bail;
use http::{HeaderMap, StatusCode, header};
use virtx_uvm::{ImageStore, RegistryServer};

use crate::config::{Auth, Config, DEFAULT_PROJECT};
use crate::endpoints::Home;
use crate::oidc::Oidc;

/// The header a request names its project in.
pub const PROJECT_HEADER: &str = "virtx-project";

/// Who a request is sent by where no one is authenticated.
pub const ANYONE: &str = "anyone";

/// Where a home's images are served as a registry.
pub const REGISTRY_PATH: &str = "/images/oci";

/// What [`Tenants`] answer, once they have checked a token.
pub type Checked<'a, T> = Pin<Box<dyn Future<Output = Result<T, StatusCode>> + Send + 'a>>;

/// Whose a server's tokens are, and the projects they reach.
pub trait Tenants: Send + Sync {
    /// Who a request carrying `token`, naming project `named` if it names one, is sent by,
    /// and the home it reaches: `401` for a token that is no one's, `400` naming no project
    /// where one must be named, and `403` naming one its sender does not reach.
    fn home<'a>(
        &'a self,
        token: Option<&'a str>,
        named: Option<&'a str>,
    ) -> Checked<'a, (String, Arc<Home>)>;

    /// Every home there is.
    fn homes(&self) -> Vec<Arc<Home>>;
}

pub struct Access {
    /// The `Origin`s a browser may call from.
    origins: Vec<String>,
    tenants: Arc<dyn Tenants>,
}

impl Access {
    /// Requests from where `config` lets a browser call, to `tenants`.
    pub fn new(config: &Config, tenants: Arc<dyn Tenants>) -> Access {
        Access {
            origins: config.origins.clone(),
            tenants,
        }
    }

    /// Every home there is.
    pub fn homes(&self) -> Vec<Arc<Home>> {
        self.tenants.homes()
    }

    /// `403` for a request with `headers` from an `Origin` not let in.
    pub fn origin(&self, headers: &HeaderMap) -> Result<(), StatusCode> {
        if let Some(origin) = headers.get(header::ORIGIN)
            && !self
                .origins
                .iter()
                .any(|allowed| allowed.as_bytes() == origin.as_bytes())
        {
            return Err(StatusCode::FORBIDDEN);
        }
        Ok(())
    }

    /// Who a request with `headers` is sent by, and the home it reaches: `403` from an
    /// `Origin` not let in, and otherwise as [`Tenants::home`] says.
    pub async fn home(&self, headers: &HeaderMap) -> Result<(String, Arc<Home>), StatusCode> {
        self.origin(headers)?;
        let named = project_named(headers)?;
        self.tenants.home(bearer(headers).ok(), named).await
    }
}

/// The bearer token `headers` carry: `401` without one.
pub fn bearer(headers: &HeaderMap) -> Result<&str, StatusCode> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .ok_or(StatusCode::UNAUTHORIZED)
}

/// The project `headers` name, if any: `400` for one that is no string.
pub fn project_named(headers: &HeaderMap) -> Result<Option<&str>, StatusCode> {
    headers
        .get(PROJECT_HEADER)
        .map(|value| value.to_str().map_err(|_| StatusCode::BAD_REQUEST))
        .transpose()
}

/// The projects a config names.
pub struct Projects {
    /// Who a token is, if the config names a provider: without one, requests are let
    /// through without credentials.
    oidc: Option<Oidc>,
    /// Each project's members, and its home.
    projects: HashMap<String, (HashSet<String>, Arc<Home>)>,
}

impl Projects {
    /// The projects `config` names, each project's home made where it says. Naming no
    /// provider, it authenticates no one, and has the project [`DEFAULT_PROJECT`] if it names
    /// none: refused listening off the loopback, unless the config says so.
    pub fn new(config: &Config) -> anyhow::Result<Projects> {
        match (&config.auth, &config.oidc) {
            (Some(Auth::None), Some(_)) => bail!("auth is none, with oidc to authenticate"),
            (None, None) if !config.listen.ip().is_loopback() => bail!(
                "listening at {} for other machines with no oidc: \
                 say `\"auth\": \"none\"` to serve them without authentication",
                config.listen
            ),
            _ => {}
        }
        let root = config.home().join("project");
        let project = |name: &str, members: &[String]| -> anyhow::Result<_> {
            let dir = root.join(name);
            let images = ImageStore::new(dir.join("images"))?;
            let registry = Arc::new(RegistryServer::new(images.clone(), REGISTRY_PATH)?);
            let members: HashSet<String> = members.iter().cloned().collect();
            Ok((members, Arc::new(Home::new(images, registry, &dir)?)))
        };
        let mut projects = HashMap::new();
        for (name, named) in &config.projects {
            projects.insert(name.clone(), project(name, &named.members)?);
        }
        if config.oidc.is_none() && projects.is_empty() {
            projects.insert(DEFAULT_PROJECT.to_owned(), project(DEFAULT_PROJECT, &[])?);
        }
        Ok(Projects {
            oidc: config.oidc.clone().map(Oidc::new),
            projects,
        })
    }

    /// Whether requests are let through without credentials.
    pub fn open(&self) -> bool {
        self.oidc.is_none()
    }

    /// The user `token` is: `401` unless the provider signed it.
    async fn user(&self, token: Option<&str>) -> Result<String, StatusCode> {
        let Some(oidc) = &self.oidc else {
            return Ok(ANYONE.to_owned());
        };
        let token = token.ok_or(StatusCode::UNAUTHORIZED)?;
        oidc.user(token).await.ok_or(StatusCode::UNAUTHORIZED)
    }
}

impl Tenants for Projects {
    /// `403` for a project the user is not a member of -- or none at all, which is not told
    /// apart. Where no one is authenticated, everyone is a member, and a request naming no
    /// project reaches [`DEFAULT_PROJECT`].
    fn home<'a>(
        &'a self,
        token: Option<&'a str>,
        named: Option<&'a str>,
    ) -> Checked<'a, (String, Arc<Home>)> {
        Box::pin(async move {
            let user = self.user(token).await?;
            let project = named
                .or(self.open().then_some(DEFAULT_PROJECT))
                .ok_or(StatusCode::BAD_REQUEST)?;
            self.projects
                .get(project)
                .filter(|(members, _)| self.open() || members.contains(&user))
                .map(|(_, home)| (user.clone(), Arc::clone(home)))
                .ok_or(StatusCode::FORBIDDEN)
        })
    }

    fn homes(&self) -> Vec<Arc<Home>> {
        let homes = self.projects.values();
        homes.map(|(_, home)| Arc::clone(home)).collect()
    }
}

#[cfg(test)]
mod tests {
    use http::HeaderValue;
    use serde_json::json;

    use super::*;
    use crate::oidc::tests::Provider;

    /// A server in `home` of the project `demo`, whose one member is `provider`'s alice.
    fn access(home: &std::path::Path, provider: &Provider) -> Access {
        let config = json!({
            "listen": "127.0.0.1:0",
            "home": home,
            "origins": ["https://allowed.example"],
            "oidc": { "issuer": provider.issuer, "client_id": "virtx" },
            "projects": { "demo": { "members": ["alice@example.com"] } },
        });
        let path = home.join("config.json");
        std::fs::write(&path, config.to_string()).unwrap();
        configured(&path).unwrap()
    }

    /// The access the config at `path` makes, over its own projects.
    fn configured(path: &std::path::Path) -> anyhow::Result<Access> {
        let config = Config::read(path)?;
        Ok(Access::new(&config, Arc::new(Projects::new(&config)?)))
    }

    fn headers(token: Option<&str>, project: Option<&str>, origin: Option<&str>) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if let Some(token) = token {
            let value = HeaderValue::from_str(&format!("Bearer {token}")).unwrap();
            headers.insert(header::AUTHORIZATION, value);
        }
        if let Some(project) = project {
            headers.insert(PROJECT_HEADER, HeaderValue::from_str(project).unwrap());
        }
        if let Some(origin) = origin {
            headers.insert(header::ORIGIN, HeaderValue::from_str(origin).unwrap());
        }
        headers
    }

    #[tokio::test]
    async fn a_member_reaches_the_project_s_home_and_no_one_else_does() {
        let dir = std::env::temp_dir().join(format!("virtx-http-access-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let provider = Provider::spawn().await;
        let access = access(&dir, &provider);
        let alice = provider.token(json!({ "email": "alice@example.com" }));
        let bob = provider.token(json!({ "email": "bob@example.com" }));
        let home =
            async |token, project, origin| access.home(&headers(token, project, origin)).await;

        let (user, reached) = home(Some(&alice), Some("demo"), None).await.unwrap();
        assert_eq!(user, "alice@example.com");
        assert_eq!(
            reached.snapshots.root(),
            dir.join("project").join("demo").as_path()
        );
        let allowed = Some("https://allowed.example");
        assert!(home(Some(&alice), Some("demo"), allowed).await.is_ok());

        let refused = async |token, project, origin| home(token, project, origin).await.err();
        let elsewhere = Some("https://evil.example");
        assert_eq!(
            refused(Some(&alice), Some("demo"), elsewhere).await,
            Some(StatusCode::FORBIDDEN)
        );
        assert_eq!(
            refused(None, Some("demo"), None).await,
            Some(StatusCode::UNAUTHORIZED)
        );
        assert_eq!(
            refused(Some("x"), Some("demo"), None).await,
            Some(StatusCode::UNAUTHORIZED)
        );
        assert_eq!(
            refused(Some(&alice), None, None).await,
            Some(StatusCode::BAD_REQUEST)
        );
        assert_eq!(
            refused(Some(&bob), Some("demo"), None).await,
            Some(StatusCode::FORBIDDEN)
        );
        assert_eq!(
            refused(Some(&alice), Some("other"), None).await,
            Some(StatusCode::FORBIDDEN)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn without_authentication_anyone_reaches_every_project_and_default_by_no_name() {
        let dir = std::env::temp_dir().join(format!("virtx-http-open-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        let config = json!({ "listen": "127.0.0.1:0", "home": dir });
        std::fs::write(&path, config.to_string()).unwrap();
        let access = configured(&path).unwrap();
        let home = async |token, project| access.home(&headers(token, project, None)).await;

        let (user, reached) = home(None, None).await.unwrap();
        assert_eq!(user, ANYONE);
        assert_eq!(
            reached.snapshots.root(),
            dir.join("project").join(DEFAULT_PROJECT).as_path()
        );
        assert!(home(Some("whatever"), Some(DEFAULT_PROJECT)).await.is_ok());
        assert_eq!(
            home(None, Some("other")).await.err(),
            Some(StatusCode::FORBIDDEN)
        );
        let elsewhere = headers(None, None, Some("https://evil.example"));
        assert_eq!(
            access.home(&elsewhere).await.err(),
            Some(StatusCode::FORBIDDEN)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn without_a_provider_it_is_open_on_the_loopback_alone_unless_it_says_so() {
        let dir = std::env::temp_dir().join(format!("virtx-http-rule-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let projects = |mut config: serde_json::Value| {
            config["home"] = json!(dir);
            let path = dir.join("config.json");
            std::fs::write(&path, config.to_string()).unwrap();
            Projects::new(&Config::read(&path).unwrap())
        };

        let local = projects(json!({ "listen": "127.0.0.1:0" })).unwrap();
        assert!(local.open());
        assert!(local.projects.contains_key(DEFAULT_PROJECT));

        let refused = projects(json!({ "listen": "0.0.0.0:0" })).err().unwrap();
        assert!(refused.to_string().contains("auth"), "{refused}");
        let said = projects(json!({ "listen": "0.0.0.0:0", "auth": "none" })).unwrap();
        assert!(said.open());

        let oidc = json!({ "issuer": "https://idp.example", "client_id": "virtx" });
        let checked = projects(json!({ "listen": "0.0.0.0:0", "oidc": oidc })).unwrap();
        assert!(!checked.open());
        assert!(checked.projects.is_empty());
        let both = json!({ "listen": "127.0.0.1:0", "oidc": oidc, "auth": "none" });
        assert!(projects(both).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
