//! Who may send a request, and which project's [`Home`] it reaches: every request carries a
//! bearer token and names its project, and is let through only for a member of it. A project's
//! sandboxes are its members' together.
//!
//! A token no user of the config's has may be one of [`Tenants`] given beside it: they say
//! which project it names, and make its home. virtx cloud's node takes its gateway's tokens so.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use futures_util::future::BoxFuture;
use http::{HeaderMap, StatusCode, header};
use virtx_uvm::{ImageStore, RegistryServer};

use crate::config::{Config, hashed};
use crate::endpoints::Home;

/// The header a request names its project in.
pub const PROJECT_HEADER: &str = "virtx-project";

/// Who the tokens no user of the config's has are, besides.
pub trait Tenants: Send + Sync {
    /// What a request carrying `token`, naming project `named` if it names one, is sent as,
    /// and the home it reaches -- made the first time: `401` for a token that is none of
    /// these, `403` for one that names another project.
    fn home<'a>(
        &'a self,
        token: &'a str,
        named: Option<&'a str>,
    ) -> BoxFuture<'a, Result<(String, Arc<Home>), StatusCode>>;

    /// Every home they have made.
    fn homes(&self) -> Vec<Arc<Home>>;
}

pub struct Access {
    /// Each user, by the SHA-256 of their token.
    users: HashMap<String, String>,
    /// The `Origin`s a browser may call from.
    origins: Vec<String>,
    /// Each project's members, and its home.
    projects: HashMap<String, (HashSet<String>, Arc<Home>)>,
    /// Whose the other tokens are, if anyone's.
    tenants: Option<Arc<dyn Tenants>>,
    /// The images the projects the config names share, and the registry they are served as.
    images: ImageStore,
    registry: Arc<RegistryServer>,
    root: PathBuf,
}

impl Access {
    /// The users and projects `config` names, each project's home made where it says.
    pub fn new(config: &Config) -> anyhow::Result<Access> {
        let root = config.home();
        let images = ImageStore::new(root.join("images"))?;
        let mut projects = HashMap::new();
        for (name, project) in &config.projects {
            let dir = root.join("project").join(name);
            let home = Home::new(images.clone(), &dir, None)?;
            let members = project.members.iter().cloned().collect();
            projects.insert(name.clone(), (members, Arc::new(home)));
        }
        Ok(Access {
            users: config.users(),
            origins: config.origins.clone(),
            projects,
            tenants: None,
            registry: Arc::new(RegistryServer::new(images.clone(), "/images/oci")?),
            images,
            root,
        })
    }

    /// As [`Access::new`], the tokens no user has taken by `tenants`.
    pub fn with_tenants(config: &Config, tenants: Arc<dyn Tenants>) -> anyhow::Result<Access> {
        Ok(Access {
            tenants: Some(tenants),
            ..Access::new(config)?
        })
    }

    /// Where everything is kept.
    pub fn root(&self) -> &std::path::Path {
        &self.root
    }

    /// Every project's home: those the config names, and those its tenants have made.
    pub fn homes(&self) -> Vec<Arc<Home>> {
        let named = self.projects.values().map(|(_, home)| Arc::clone(home));
        let theirs = self.tenants.iter().flat_map(|tenants| tenants.homes());
        named.chain(theirs).collect()
    }

    /// The images the projects the config names share.
    pub fn images(&self) -> &ImageStore {
        &self.images
    }

    /// The registry a request to `/images/v2/` with `headers` reaches: for a user, the images
    /// the config's projects share; for a tenant, its project's own, as [`home`](Self::home)
    /// finds it -- with the home.
    pub async fn registry(
        &self,
        headers: &HeaderMap,
    ) -> Result<(Arc<RegistryServer>, Option<Arc<Home>>), StatusCode> {
        self.origin(headers)?;
        let token = bearer(headers)?;
        if self.users.contains_key(&hashed(token)) {
            return Ok((Arc::clone(&self.registry), None));
        }
        let (_, home) = self.tenant(token, project_named(headers)?).await?;
        Ok((home.registry(), Some(home)))
    }

    /// The user a request with `headers` is sent by, and the home it reaches: `403` from an
    /// `Origin` not let in, `401` without a token this server knows, `400` naming no project,
    /// and `403` naming one the user is not a member of -- or none at all, which is not told
    /// apart.
    ///
    /// A tenant's token is no user's: its project is the tenant's to say, and so is what the
    /// request is sent as -- which every request for it with the same shares, cancelling
    /// included.
    pub async fn home(&self, headers: &HeaderMap) -> Result<(String, Arc<Home>), StatusCode> {
        self.origin(headers)?;
        let token = bearer(headers)?;
        let named = project_named(headers)?;
        let Some(user) = self.users.get(&hashed(token)).cloned() else {
            return self.tenant(token, named).await;
        };
        let project = named.ok_or(StatusCode::BAD_REQUEST)?;
        self.projects
            .get(project)
            .filter(|(members, _)| members.contains(&user))
            .map(|(_, home)| (user.clone(), Arc::clone(home)))
            .ok_or(StatusCode::FORBIDDEN)
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

    async fn tenant(
        &self,
        token: &str,
        named: Option<&str>,
    ) -> Result<(String, Arc<Home>), StatusCode> {
        match &self.tenants {
            Some(tenants) => tenants.home(token, named).await,
            None => Err(StatusCode::UNAUTHORIZED),
        }
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

#[cfg(test)]
mod tests {
    use http::HeaderValue;

    use super::*;

    fn access(home: &std::path::Path) -> Access {
        let config = format!(
            r#"{{
                "listen": "127.0.0.1:0",
                "home": {home:?},
                "origins": ["https://allowed.example"],
                "users": {{
                    "alice": {{ "token_sha256": "{}" }},
                    "bob": {{ "token_sha256": "{}" }}
                }},
                "projects": {{ "demo": {{ "members": ["alice"] }} }}
            }}"#,
            hashed("a-token"),
            hashed("b-token"),
        );
        let path = home.join("config.json");
        std::fs::write(&path, config).unwrap();
        Access::new(&Config::read(&path).unwrap()).unwrap()
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
        let access = access(&dir);
        let home =
            async |token, project, origin| access.home(&headers(token, project, origin)).await;

        let (user, reached) = home(Some("a-token"), Some("demo"), None).await.unwrap();
        assert_eq!(user, "alice");
        assert_eq!(
            reached.snapshots.root(),
            dir.join("project").join("demo").as_path()
        );
        assert!(
            home(
                Some("a-token"),
                Some("demo"),
                Some("https://allowed.example")
            )
            .await
            .is_ok()
        );

        let refused = async |token, project, origin| home(token, project, origin).await.err();
        assert_eq!(
            refused(Some("a-token"), Some("demo"), Some("https://evil.example")).await,
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
            refused(Some("a-token"), None, None).await,
            Some(StatusCode::BAD_REQUEST)
        );
        assert_eq!(
            refused(Some("b-token"), Some("demo"), None).await,
            Some(StatusCode::FORBIDDEN)
        );
        assert_eq!(
            refused(Some("a-token"), Some("other"), None).await,
            Some(StatusCode::FORBIDDEN)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
