//! Who may send a request, and which project's [`Home`] it reaches. [`Access`] lets a request
//! in from where a browser may call, asks its [`Authenticator`] who the token is and which
//! project it reaches, and takes that project's home from its [`Homes`].
//!
//! The server's own authenticator is [`Projects`]: the projects its config names, each to its
//! members among the users of the config's OpenID Connect provider ([`Oidc`]). A server built on
//! this one gives its own.
//!
//! [`MemberProjects`] keeps each project's members beside its root, in
//! `<home>/project/<name>/members.json`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::future::Future;
use std::io;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use futures_util::future::BoxFuture;
use http::{HeaderMap, StatusCode, header};
use serde::{Deserialize, Serialize};
use virtx_core::{DEFAULT_PROJECT, PROJECT_HEADER, Role};
use virtx_uvm::{
    ImageStore, ProjectError, ProjectStore, RegistryServer, is_project_name, project_names,
    project_root,
};

use crate::config::Config;
use crate::endpoints::Home;
use crate::oidc::Oidc;

/// Where a home's images are served as a registry.
pub const REGISTRY_PATH: &str = "/images/oci";

/// What an [`Authenticator`] answers, once it has checked a token.
pub type Checked<'a, T> = Pin<Box<dyn Future<Output = Result<T, StatusCode>> + Send + 'a>>;

/// Who a server's tokens are, and the projects they reach.
pub trait Authenticator: Send + Sync {
    /// Who a request carrying `token`, naming project `named` if it names one, is sent by,
    /// and the project it reaches: `401` for a token that is no one's, and `403` for a project
    /// its sender does not reach.
    fn authenticate<'a>(
        &'a self,
        token: Option<&'a str>,
        named: Option<&'a str>,
    ) -> Checked<'a, (String, String)>;
}

/// Each project's home, in its [own root](project_root) under a root, made as it is first
/// reached.
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
        let dir = project_root(&self.root, project)?;
        let images = ImageStore::new(dir.join("images"))?;
        let registry = Arc::new(RegistryServer::new(images.clone(), REGISTRY_PATH)?);
        let home = Arc::new(Home::new(images, registry, &dir)?);
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
    authenticator: Arc<dyn Authenticator>,
    homes: Arc<Homes>,
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
            authenticator,
            homes,
        }
    }

    pub fn origins(&self) -> &[String] {
        &self.origins
    }

    /// Every home made.
    pub fn homes(&self) -> Vec<Arc<Home>> {
        self.homes.all()
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
    /// `Origin` not let in, or for a project that can be no project's name, and otherwise as
    /// [`Authenticator::authenticate`] says.
    pub async fn home(&self, headers: &HeaderMap) -> Result<(String, Arc<Home>), StatusCode> {
        self.origin(headers)?;
        let named = project_named(headers)?;
        let (user, project) = self
            .authenticator
            .authenticate(bearer(headers), named)
            .await?;
        if !is_project_name(&project) {
            return Err(StatusCode::FORBIDDEN);
        }
        let home = self.homes.home(&project).map_err(|e| {
            eprintln!("making project {project}'s home: {e:#}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
        Ok((user, home))
    }
}

/// The bearer token `headers` carry, if any.
pub fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
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
    /// Who a token is.
    oidc: Oidc,
    /// Each project's members.
    members: HashMap<String, HashSet<String>>,
}

impl Projects {
    /// The projects `config` names, each one's home made in `homes`.
    pub fn new(config: &Config, homes: &Homes) -> anyhow::Result<Projects> {
        let members: HashMap<String, HashSet<String>> = config
            .projects
            .iter()
            .map(|(name, named)| (name.clone(), named.members.iter().cloned().collect()))
            .collect();
        // Made now, not as first reached: a project's sandboxes left from before are swept as
        // the server starts.
        for name in members.keys() {
            homes.home(name)?;
        }
        Ok(Projects {
            oidc: Oidc::new(config.oidc.clone()),
            members,
        })
    }

    /// The user `token` is: `401` unless the provider signed it.
    async fn user(&self, token: Option<&str>) -> Result<String, StatusCode> {
        let token = token.ok_or(StatusCode::UNAUTHORIZED)?;
        self.oidc.user(token).await.ok_or(StatusCode::UNAUTHORIZED)
    }
}

impl Authenticator for Projects {
    /// `403` for a project the user is not a member of -- or none at all, which is not told
    /// apart. A request naming no project reaches [`DEFAULT_PROJECT`].
    fn authenticate<'a>(
        &'a self,
        token: Option<&'a str>,
        named: Option<&'a str>,
    ) -> Checked<'a, (String, String)> {
        Box::pin(async move {
            let user = self.user(token).await?;
            let project = named.unwrap_or(DEFAULT_PROJECT);
            match self.members.get(project) {
                Some(members) if members.contains(&user) => Ok((user, project.to_owned())),
                _ => Err(StatusCode::FORBIDDEN),
            }
        })
    }
}

/// The projects under a home that have members, each one's in its `members.json`: a project is
/// there while that file is.
pub struct MemberProjects {
    home: PathBuf,
    /// Held while a project's members are read to be changed, and written.
    changing: Mutex<()>,
}

/// `members.json`, as kept.
#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Members {
    members: BTreeMap<String, Role>,
}

impl MemberProjects {
    pub fn new(home: impl Into<PathBuf>) -> MemberProjects {
        MemberProjects {
            home: home.into(),
            changing: Mutex::default(),
        }
    }

    fn file(&self, project: &str) -> Result<PathBuf, ProjectError> {
        Ok(project_root(&self.home, project)?.join("members.json"))
    }

    /// The members of `project`, or `None` for a project not here.
    fn read(&self, project: &str) -> Result<Option<Members>, ProjectError> {
        let file = self.file(project)?;
        match fs::read(&file) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|e| ProjectError::io(format!("reading {}", file.display()), e.into())),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(ProjectError::io(format!("reading {}", file.display()), e)),
        }
    }

    /// Put `members` in place for `project` whole: written beside, and renamed over.
    fn write(&self, project: &str, members: &Members) -> Result<(), ProjectError> {
        let file = self.file(project)?;
        let part = file.with_extension("json.part");
        let bytes = serde_json::to_vec_pretty(members).expect("a map of strings");
        fs::write(&part, bytes)
            .map_err(|e| ProjectError::io(format!("writing {}", part.display()), e))?;
        fs::rename(&part, &file)
            .map_err(|e| ProjectError::io(format!("writing {}", file.display()), e))
    }

    fn members_of(&self, project: &str) -> Result<Members, ProjectError> {
        self.read(project)?
            .ok_or_else(|| ProjectError::Unknown(project.to_owned()))
    }
}

impl ProjectStore for MemberProjects {
    fn role<'a>(
        &'a self,
        user: &'a str,
        project: &'a str,
    ) -> BoxFuture<'a, Result<Option<Role>, ProjectError>> {
        Box::pin(async move {
            if !is_project_name(project) {
                return Ok(None);
            }
            Ok(self
                .read(project)?
                .and_then(|members| members.members.get(user).copied()))
        })
    }

    fn projects<'a>(
        &'a self,
        user: &'a str,
    ) -> BoxFuture<'a, Result<Vec<(String, Role)>, ProjectError>> {
        Box::pin(async move {
            let mut reached = Vec::new();
            for name in project_names(&self.home)? {
                if let Some(role) = self
                    .read(&name)?
                    .and_then(|members| members.members.get(user).copied())
                {
                    reached.push((name, role));
                }
            }
            reached.sort();
            Ok(reached)
        })
    }

    fn create<'a>(
        &'a self,
        project: &'a str,
        owner: &'a str,
    ) -> BoxFuture<'a, Result<(), ProjectError>> {
        Box::pin(async move {
            let dir = project_root(&self.home, project)?;
            let _changing = self.changing.lock().unwrap();
            if self.read(project)?.is_some() {
                return Err(ProjectError::AlreadyExists(project.to_owned()));
            }
            fs::create_dir_all(&dir)
                .map_err(|e| ProjectError::io(format!("making {}", dir.display()), e))?;
            let members = Members {
                members: BTreeMap::from([(owner.to_owned(), Role::Owner)]),
            };
            self.write(project, &members)
        })
    }

    fn remove<'a>(&'a self, project: &'a str) -> BoxFuture<'a, Result<(), ProjectError>> {
        Box::pin(async move {
            let file = self.file(project)?;
            let _changing = self.changing.lock().unwrap();
            match fs::remove_file(&file) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    Err(ProjectError::Unknown(project.to_owned()))
                }
                Err(e) => Err(ProjectError::io(format!("removing {}", file.display()), e)),
            }
        })
    }

    fn members<'a>(
        &'a self,
        project: &'a str,
    ) -> BoxFuture<'a, Result<Vec<(String, Role)>, ProjectError>> {
        Box::pin(async move { Ok(self.members_of(project)?.members.into_iter().collect()) })
    }

    fn set_member<'a>(
        &'a self,
        project: &'a str,
        user: &'a str,
        role: Option<Role>,
    ) -> BoxFuture<'a, Result<(), ProjectError>> {
        Box::pin(async move {
            let _changing = self.changing.lock().unwrap();
            let mut members = self.members_of(project)?;
            match role {
                Some(role) => members.members.insert(user.to_owned(), role),
                None => members.members.remove(user),
            };
            if !members.members.values().any(|role| *role == Role::Owner) {
                return Err(ProjectError::NoOwner(project.to_owned()));
            }
            self.write(project, &members)
        })
    }
}

#[cfg(test)]
mod tests {
    use http::HeaderValue;
    use serde_json::json;

    use super::*;

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
    async fn a_request_carries_a_provider_s_token_from_where_a_browser_is_let_in() {
        let dir = std::env::temp_dir().join(format!("virtx-tcp-access-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let config = Config::parse(json!({
            "listen": "127.0.0.1:0",
            "home": dir,
            "oidc": { "issuer": "https://idp.example", "client_id": "virtx" },
            "projects": { "demo": { "members": ["alice@example.com"] } },
        }))
        .unwrap();
        let homes = Arc::new(Homes::new(config.home()));
        let projects = Arc::new(Projects::new(&config, &homes).unwrap());
        let access = Access::new(config.origins, projects, homes);
        // The config's projects are made as the server starts.
        assert!(dir.join("project/demo").is_dir());

        let home = async |token, origin| {
            access
                .home(&headers(token, Some("demo"), origin))
                .await
                .err()
        };
        assert_eq!(home(None, None).await, Some(StatusCode::UNAUTHORIZED));
        assert_eq!(
            home(Some("not a token"), None).await,
            Some(StatusCode::UNAUTHORIZED)
        );
        assert_eq!(
            home(None, Some("https://evil.example")).await,
            Some(StatusCode::FORBIDDEN)
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_config_names_its_provider_and_its_projects_in_lower_case() {
        let refused = Config::parse(json!({ "listen": "127.0.0.1:0" }))
            .err()
            .unwrap();
        assert!(refused.to_string().contains("oidc"), "{refused}");
        // Two names that differ in case alone would be one directory where case is ignored.
        let oidc = json!({ "issuer": "https://idp.example", "client_id": "virtx" });
        let cased = json!({ "listen": "127.0.0.1:0", "oidc": oidc, "projects": { "Demo": {} } });
        let refused = Config::parse(cased).err().unwrap();
        assert!(refused.to_string().contains("lower-case"), "{refused}");
    }

    #[tokio::test]
    async fn members_reach_their_projects_in_their_roles_and_one_owner_stays() {
        let home = std::env::temp_dir().join(format!("virtx-tcp-members-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        let store = MemberProjects::new(&home);

        assert_eq!(store.role("alice", DEFAULT_PROJECT).await.unwrap(), None);
        store.create("demo", "alice").await.unwrap();
        assert!(home.join("project/demo/members.json").is_file());
        assert!(matches!(
            store.create("demo", "bob").await,
            Err(ProjectError::AlreadyExists(_))
        ));
        assert!(matches!(
            store.create("Demo", "bob").await,
            Err(ProjectError::InvalidName(_))
        ));
        assert_eq!(
            store.role("alice", "demo").await.unwrap(),
            Some(Role::Owner)
        );
        assert_eq!(store.role("bob", "demo").await.unwrap(), None);

        store
            .set_member("demo", "bob", Some(Role::Member))
            .await
            .unwrap();
        store.create("other", "bob").await.unwrap();
        assert_eq!(
            store.projects("bob").await.unwrap(),
            [
                ("demo".to_owned(), Role::Member),
                ("other".to_owned(), Role::Owner)
            ]
        );

        // The last owner neither leaves nor steps down.
        assert!(matches!(
            store.set_member("demo", "alice", None).await,
            Err(ProjectError::NoOwner(_))
        ));
        assert!(matches!(
            store.set_member("demo", "alice", Some(Role::Member)).await,
            Err(ProjectError::NoOwner(_))
        ));
        store
            .set_member("demo", "bob", Some(Role::Owner))
            .await
            .unwrap();
        store.set_member("demo", "alice", None).await.unwrap();

        // Kept on disk, for the next store over the same home.
        let again = MemberProjects::new(&home);
        assert_eq!(
            again.members("demo").await.unwrap(),
            [("bob".to_owned(), Role::Owner)]
        );

        again.remove("demo").await.unwrap();
        assert_eq!(again.role("bob", "demo").await.unwrap(), None);
        assert!(matches!(
            again.members("demo").await,
            Err(ProjectError::Unknown(_))
        ));
        assert_eq!(
            again.projects("bob").await.unwrap(),
            [("other".to_owned(), Role::Owner)]
        );
        let _ = fs::remove_dir_all(&home);
    }
}
