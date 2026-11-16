//! Who may send a request, and which project's [`Home`] it reaches. [`Access`] lets a request
//! in from where a browser may call, asks its [`Authenticator`] who the token is and which
//! project it reaches, and takes that project's home from its [`Homes`].
//!
//! The server's own authenticator is [`Projects`]: the projects its config names, each to its
//! members among the users of the config's OpenID Connect provider ([`Oidc`]) -- or, of a
//! config that names no provider, to [`ANYONE`]. A server built on this one gives its own.
//!
//! Or it gives [`Tenants`] in place of both, who say who a token is and make the home it
//! reaches themselves ([`Access::with_tenants`]): virtx cloud's node takes its gateway's tokens
//! so, each project's home kept past the node.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use anyhow::bail;
use http::{HeaderMap, StatusCode, header};
use virtx_uvm::ImageStore;

use crate::config::{Auth, Config, DEFAULT_PROJECT};
use crate::endpoints::Home;
use crate::oidc::Oidc;

/// The header a request names its project in.
pub const PROJECT_HEADER: &str = "virtx-project";

/// Who a request is sent by where no one is authenticated.
pub const ANYONE: &str = "anyone";

/// Where a home's images are served as a registry.
pub const REGISTRY_PATH: &str = "/images/oci";

/// What an [`Authenticator`] answers, once it has checked a token.
pub type Checked<'a, T> = Pin<Box<dyn Future<Output = Result<T, StatusCode>> + Send + 'a>>;

/// Who a server's tokens are, and the projects they reach.
pub trait Authenticator: Send + Sync {
    /// Who a request carrying `token`, naming project `named` if it names one, is sent by,
    /// and the project it reaches: `401` for a token that is no one's, `400` naming no project
    /// where one must be named, and `403` naming one its sender does not reach.
    fn authenticate<'a>(
        &'a self,
        token: Option<&'a str>,
        named: Option<&'a str>,
    ) -> Checked<'a, (String, String)>;
}

/// Who a server's tokens are, and the homes they reach, which they make: in place of an
/// [`Authenticator`] and [`Homes`].
pub trait Tenants: Send + Sync {
    /// What a request carrying `token`, naming project `named` if it names one, is sent as,
    /// and the home it reaches -- made the first time: `401` for a token that is none of
    /// these, `403` for one that names another project.
    fn home<'a>(
        &'a self,
        token: &'a str,
        named: Option<&'a str>,
    ) -> Checked<'a, (String, Arc<Home>)>;

    /// Every home they have made.
    fn homes(&self) -> Vec<Arc<Home>>;
}

/// Each project's home, in a directory of its name under a root, made as it is first reached.
pub struct Homes {
    root: PathBuf,
    made: Mutex<HashMap<String, Arc<Home>>>,
}

impl Homes {
    pub fn new(root: PathBuf) -> Homes {
        Homes {
            root,
            made: Mutex::default(),
        }
    }

    /// Project `project`'s home, its images its own.
    pub fn home(&self, project: &str) -> anyhow::Result<Arc<Home>> {
        let mut made = self.made.lock().unwrap();
        if let Some(home) = made.get(project) {
            return Ok(Arc::clone(home));
        }
        let dir = self.root.join(project);
        let images = ImageStore::new(dir.join("images"))?;
        let home = Arc::new(Home::new(images, &dir, None)?);
        made.insert(project.to_owned(), Arc::clone(&home));
        Ok(home)
    }

    /// Every home made.
    pub fn all(&self) -> Vec<Arc<Home>> {
        self.made.lock().unwrap().values().cloned().collect()
    }
}

pub struct Access {
    /// The `Origin`s a browser may call from.
    origins: Vec<String>,
    who: Who,
}

/// Who says who a token is, and makes the home it reaches.
enum Who {
    Authenticated {
        authenticator: Arc<dyn Authenticator>,
        homes: Arc<Homes>,
    },
    Tenants(Arc<dyn Tenants>),
}

impl Access {
    /// Requests from the `origins` a browser may call from, let in by `authenticator` to `homes`.
    pub fn new(
        origins: Vec<String>,
        authenticator: Arc<dyn Authenticator>,
        homes: Arc<Homes>,
    ) -> Access {
        Access {
            origins,
            who: Who::Authenticated {
                authenticator,
                homes,
            },
        }
    }

    /// Requests from the `origins` of `config` a browser may call from, each token's sender and
    /// home as `tenants` say.
    pub fn with_tenants(config: &Config, tenants: Arc<dyn Tenants>) -> anyhow::Result<Access> {
        Ok(Access {
            origins: config.origins.clone(),
            who: Who::Tenants(tenants),
        })
    }

    pub fn origins(&self) -> &[String] {
        &self.origins
    }

    /// Every home made.
    pub fn homes(&self) -> Vec<Arc<Home>> {
        match &self.who {
            Who::Authenticated { homes, .. } => homes.all(),
            Who::Tenants(tenants) => tenants.homes(),
        }
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
    /// `Origin` not let in, and otherwise as [`Authenticator::authenticate`] says -- or, with
    /// [`Tenants`], `401` without a token, and otherwise as [`Tenants::home`] says.
    pub async fn home(&self, headers: &HeaderMap) -> Result<(String, Arc<Home>), StatusCode> {
        self.origin(headers)?;
        let named = project_named(headers)?;
        let (authenticator, homes) = match &self.who {
            Who::Authenticated {
                authenticator,
                homes,
            } => (authenticator, homes),
            Who::Tenants(tenants) => return tenants.home(bearer(headers)?, named).await,
        };
        let (user, project) = authenticator
            .authenticate(bearer(headers).ok(), named)
            .await?;
        let home = homes.home(&project).map_err(|e| {
            eprintln!("making project {project}'s home: {e:#}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
        Ok((user, home))
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
    /// Each project's members.
    members: HashMap<String, HashSet<String>>,
}

impl Projects {
    /// The projects `config` names, each one's home made in `homes`. Naming no provider, it
    /// authenticates no one, and has the project [`DEFAULT_PROJECT`] if it names none:
    /// refused listening off the loopback, unless the config says so.
    pub fn new(config: &Config, homes: &Homes) -> anyhow::Result<Projects> {
        match (&config.auth, &config.oidc) {
            (Some(Auth::None), Some(_)) => bail!("auth is none, with oidc to authenticate"),
            (None, None) if !config.listen.ip().is_loopback() => bail!(
                "listening at {} for other machines with no oidc: \
                 say `\"auth\": \"none\"` to serve them without authentication",
                config.listen
            ),
            _ => {}
        }
        let mut members: HashMap<String, HashSet<String>> = config
            .projects
            .iter()
            .map(|(name, named)| (name.clone(), named.members.iter().cloned().collect()))
            .collect();
        if config.oidc.is_none() && members.is_empty() {
            members.insert(DEFAULT_PROJECT.to_owned(), HashSet::new());
        }
        // Made now, not as first reached: a project's sandboxes left from before are swept as
        // the server starts.
        for name in members.keys() {
            homes.home(name)?;
        }
        Ok(Projects {
            oidc: config.oidc.clone().map(Oidc::new),
            members,
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

impl Authenticator for Projects {
    /// `403` for a project the user is not a member of -- or none at all, which is not told
    /// apart. Where no one is authenticated, everyone is a member, and a request naming no
    /// project reaches [`DEFAULT_PROJECT`].
    fn authenticate<'a>(
        &'a self,
        token: Option<&'a str>,
        named: Option<&'a str>,
    ) -> Checked<'a, (String, String)> {
        Box::pin(async move {
            let user = self.user(token).await?;
            let project = named
                .or(self.open().then_some(DEFAULT_PROJECT))
                .ok_or(StatusCode::BAD_REQUEST)?;
            match self.members.get(project) {
                Some(members) if self.open() || members.contains(&user) => {
                    Ok((user, project.to_owned()))
                }
                _ => Err(StatusCode::FORBIDDEN),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use http::HeaderValue;
    use serde_json::json;

    use super::*;

    /// The access the config at `path` makes, over its own projects.
    fn configured(path: &std::path::Path) -> anyhow::Result<Access> {
        let config = Config::read(path)?;
        let homes = Arc::new(Homes::new(config.home().join("project")));
        let projects = Arc::new(Projects::new(&config, &homes)?);
        Ok(Access::new(config.origins, projects, homes))
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
            let config = Config::read(&path).unwrap();
            Projects::new(&config, &Homes::new(config.home().join("project")))
        };

        let local = projects(json!({ "listen": "127.0.0.1:0" })).unwrap();
        assert!(local.open());
        assert!(local.members.contains_key(DEFAULT_PROJECT));

        let refused = projects(json!({ "listen": "0.0.0.0:0" })).err().unwrap();
        assert!(refused.to_string().contains("auth"), "{refused}");
        let said = projects(json!({ "listen": "0.0.0.0:0", "auth": "none" })).unwrap();
        assert!(said.open());

        let oidc = json!({ "issuer": "https://idp.example", "client_id": "virtx" });
        let checked = projects(json!({ "listen": "0.0.0.0:0", "oidc": oidc })).unwrap();
        assert!(!checked.open());
        assert!(checked.members.is_empty());
        let both = json!({ "listen": "127.0.0.1:0", "oidc": oidc, "auth": "none" });
        assert!(projects(both).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Tenants who know one token, which reaches project `p`'s home and no other.
    struct OneToken(Arc<Home>);

    impl Tenants for OneToken {
        fn home<'a>(
            &'a self,
            token: &'a str,
            named: Option<&'a str>,
        ) -> Checked<'a, (String, Arc<Home>)> {
            Box::pin(async move {
                if token != "t" {
                    return Err(StatusCode::UNAUTHORIZED);
                }
                if named.is_some_and(|named| named != "p") {
                    return Err(StatusCode::FORBIDDEN);
                }
                Ok(("project:p".to_owned(), Arc::clone(&self.0)))
            })
        }

        fn homes(&self) -> Vec<Arc<Home>> {
            vec![Arc::clone(&self.0)]
        }
    }

    #[tokio::test]
    async fn with_tenants_a_token_is_theirs_to_say_and_none_is_no_one() {
        let dir = std::env::temp_dir().join(format!("virtx-http-tenants-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        let config = json!({ "listen": "127.0.0.1:0", "home": dir });
        std::fs::write(&path, config.to_string()).unwrap();
        let home = Homes::new(dir.join("project")).home("p").unwrap();
        let tenants = Arc::new(OneToken(Arc::clone(&home)));
        let access = Access::with_tenants(&Config::read(&path).unwrap(), tenants).unwrap();
        let reach = async |token, project| access.home(&headers(token, project, None)).await;

        let (user, reached) = reach(Some("t"), None).await.unwrap();
        assert_eq!(user, "project:p");
        assert!(Arc::ptr_eq(&reached, &home));
        assert_eq!(access.homes().len(), 1);
        // Where a config alone would let anyone in, no token is no one's.
        assert_eq!(
            reach(None, None).await.err(),
            Some(StatusCode::UNAUTHORIZED)
        );
        assert_eq!(
            reach(Some("x"), None).await.err(),
            Some(StatusCode::UNAUTHORIZED)
        );
        assert_eq!(
            reach(Some("t"), Some("q")).await.err(),
            Some(StatusCode::FORBIDDEN)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
