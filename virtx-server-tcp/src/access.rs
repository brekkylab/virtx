//! Who may send a request, and which project's [`Home`] it reaches: every request carries an
//! ID token of the config's OpenID Connect provider ([`Oidc`]) and names its project, and is
//! let through only for a member of it. A project's sandboxes are its members' together.
//!
//! A config that names no provider ([`Config::open`]) authenticates no one: every request
//! is let into every project, as [`ANYONE`].

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use http::{HeaderMap, StatusCode, header};
use virtx_uvm::ImageStore;

use crate::config::{Config, DEFAULT_PROJECT};
use crate::endpoints::Home;
use crate::oidc::Oidc;

/// The header a request names its project in.
pub const PROJECT_HEADER: &str = "virtx-project";

/// Who a request is sent by where no one is authenticated.
pub const ANYONE: &str = "anyone";

pub struct Access {
    /// Who a token is, if the config names a provider: without one, requests are let
    /// through without credentials.
    oidc: Option<Oidc>,
    /// The `Origin`s a browser may call from.
    origins: Vec<String>,
    /// Each project's members, and its home.
    projects: HashMap<String, (HashSet<String>, Arc<Home>)>,
    /// The images every project shares.
    images: ImageStore,
}

impl Access {
    /// The users and projects `config` names, each project's home made where it says.
    pub fn new(config: &Config) -> anyhow::Result<Access> {
        let root = config.home();
        let images = ImageStore::new(root.join("images"))?;
        let mut projects = HashMap::new();
        for (name, project) in &config.projects {
            let dir = root.join("project").join(name);
            let home = Home::new(images.clone(), &dir)?;
            let members = project.members.iter().cloned().collect();
            projects.insert(name.clone(), (members, Arc::new(home)));
        }
        Ok(Access {
            oidc: config.oidc.clone().map(Oidc::new),
            origins: config.origins.clone(),
            projects,
            images,
        })
    }

    /// Every project's home.
    pub fn homes(&self) -> impl Iterator<Item = &Arc<Home>> {
        self.projects.values().map(|(_, home)| home)
    }

    /// The images every project shares.
    pub fn images(&self) -> &ImageStore {
        &self.images
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

    /// The user a request with `headers` is sent by: `403` from an `Origin` not let in, and
    /// `401` without a token the provider signed.
    pub async fn user(&self, headers: &HeaderMap) -> Result<String, StatusCode> {
        self.origin(headers)?;
        let Some(oidc) = &self.oidc else {
            return Ok(ANYONE.to_owned());
        };
        let token = headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .ok_or(StatusCode::UNAUTHORIZED)?;
        oidc.user(token).await.ok_or(StatusCode::UNAUTHORIZED)
    }

    /// The user a request with `headers` is sent by, as [`user`](Access::user) says, and the
    /// home it reaches: `400` naming no project, and `403` naming one the user is not a member
    /// of -- or none at all, which is not told apart. Where no one is authenticated, everyone
    /// is a member, and a request naming no project reaches [`DEFAULT_PROJECT`].
    pub async fn home(&self, headers: &HeaderMap) -> Result<(String, Arc<Home>), StatusCode> {
        let user = self.user(headers).await?;
        let project = headers
            .get(PROJECT_HEADER)
            .and_then(|value| value.to_str().ok())
            .or(self.oidc.is_none().then_some(DEFAULT_PROJECT))
            .ok_or(StatusCode::BAD_REQUEST)?;
        self.projects
            .get(project)
            .filter(|(members, _)| self.oidc.is_none() || members.contains(&user))
            .map(|(_, home)| (user.clone(), Arc::clone(home)))
            .ok_or(StatusCode::FORBIDDEN)
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
        let access = Access::new(&Config::read(&path).unwrap()).unwrap();
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
}
