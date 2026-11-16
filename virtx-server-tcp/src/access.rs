//! Who may send a request, and which project's [`Home`] it reaches. [`Access`] lets a request
//! in from where a browser may call, asks its [`Identity`] who the token is, asks its
//! [`Membership`] what they are in the project the request names, and takes that project's
//! home, made as it is first reached.
//!
//! The server's own are [`Access::configured`]'s: the config's OpenID Connect provider
//! ([`Oidc`]), and each project's members kept in its directory ([`FileMembership`]). A server
//! built on this one gives its own of either, such as a membership kept in a database.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use futures_util::future::BoxFuture;
use http::{HeaderMap, StatusCode, header};
use serde::{Deserialize, Serialize};
use virtx_core::{DEFAULT_PROJECT, PROJECT_HEADER, Role};
use virtx_uvm::{ImageStore, Membership, ProjectError, ProjectStore, RegistryServer};

use crate::config::Config;
use crate::endpoints::Home;
use crate::oidc::Oidc;

/// Where a home's images are served as a registry.
pub const REGISTRY_PATH: &str = "/images/oci";

/// Who a server's tokens are.
pub trait Identity: Send + Sync {
    /// The user a request carrying `token` is sent by: `None` for a token that is no one's, or
    /// for none where one is needed.
    fn identify<'a>(&'a self, token: Option<&'a str>) -> BoxFuture<'a, Option<String>>;
}

impl Identity for Oidc {
    fn identify<'a>(&'a self, token: Option<&'a str>) -> BoxFuture<'a, Option<String>> {
        Box::pin(async move { self.user(token?).await })
    }
}

pub struct Access {
    /// The `Origin`s a browser may call from.
    origins: Vec<String>,
    identity: Arc<dyn Identity>,
    membership: Arc<dyn Membership>,
    store: ProjectStore,
    /// Each project's home, by name, made as it is first reached.
    homes: Mutex<HashMap<String, Arc<Home>>>,
}

impl Access {
    /// Requests from the `origins` a browser may call from, sent by whom `identity` says, to the
    /// projects in `store` whose members `membership` says. The home of each project in `store`
    /// is made now, so that its sandboxes left from before are swept as the server starts.
    pub fn new(
        origins: Vec<String>,
        identity: Arc<dyn Identity>,
        membership: Arc<dyn Membership>,
        store: ProjectStore,
    ) -> anyhow::Result<Access> {
        let access = Access {
            origins,
            identity,
            membership,
            store,
            homes: Mutex::default(),
        };
        for name in access.store.list()? {
            access.home_of(&name)?;
        }
        Ok(access)
    }

    /// What `config` says. The projects it names are each made, once, with its members its
    /// owners: from then on their members are the membership's, changed at `/projects/`.
    pub async fn configured(config: &Config) -> anyhow::Result<Access> {
        let store = ProjectStore::new(config.home());
        let membership = FileMembership::new(store.clone());
        for (name, named) in &config.projects {
            let Some((owner, others)) = named.members.split_first() else {
                continue;
            };
            match membership.create(name, owner).await {
                Ok(()) => {
                    for member in others {
                        membership
                            .set_member(name, member, Some(Role::Owner))
                            .await?;
                    }
                }
                Err(ProjectError::AlreadyExists(_)) => {}
                Err(e) => return Err(e.into()),
            }
        }
        if !config.projects.is_empty() {
            eprintln!(
                "the config's projects are made once, and each one's members are its own from \
                 then on: change them at /projects/, not in the config"
            );
        }
        Access::new(
            config.origins.clone(),
            Arc::new(Oidc::new(config.oidc.clone())),
            Arc::new(membership),
            store,
        )
    }

    pub fn origins(&self) -> &[String] {
        &self.origins
    }

    pub fn membership(&self) -> &dyn Membership {
        &*self.membership
    }

    pub fn store(&self) -> &ProjectStore {
        &self.store
    }

    /// Every home made.
    pub fn homes(&self) -> Vec<Arc<Home>> {
        self.homes.lock().unwrap().values().cloned().collect()
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

    /// Who a request with `headers` is sent by: `403` from an `Origin` not let in, and `401`
    /// for a token that is no one's.
    pub async fn user(&self, headers: &HeaderMap) -> Result<String, StatusCode> {
        self.origin(headers)?;
        self.identity
            .identify(bearer(headers))
            .await
            .ok_or(StatusCode::UNAUTHORIZED)
    }

    /// Who a request with `headers` is sent by, and the home of the project it names, or of
    /// [`DEFAULT_PROJECT`] where it names none: as [`user`](Access::user) refuses, `400` for a
    /// project that is no string, and `403` for one its sender is no member of -- or that does
    /// not exist, which is not told apart.
    pub async fn home(&self, headers: &HeaderMap) -> Result<(String, Arc<Home>), StatusCode> {
        let user = self.user(headers).await?;
        let project = match headers.get(PROJECT_HEADER) {
            Some(named) => named.to_str().map_err(|_| StatusCode::BAD_REQUEST)?,
            None => DEFAULT_PROJECT,
        };
        match self.membership.role(&user, project).await {
            Ok(Some(_)) => {}
            Ok(None) => return Err(StatusCode::FORBIDDEN),
            Err(e) => {
                eprintln!("reading project {project}'s members: {e}");
                return Err(StatusCode::INTERNAL_SERVER_ERROR);
            }
        }
        let home = self.home_of(project).map_err(|e| {
            eprintln!("making project {project}'s home: {e:#}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
        Ok((user, home))
    }

    /// Project `project`'s home, its images its own.
    fn home_of(&self, project: &str) -> anyhow::Result<Arc<Home>> {
        let mut homes = self.homes.lock().unwrap();
        if let Some(home) = homes.get(project) {
            return Ok(Arc::clone(home));
        }
        let dir = self.store.path(project)?;
        let images = ImageStore::new(dir.join("images"))?;
        let registry = Arc::new(RegistryServer::new(images.clone(), REGISTRY_PATH)?);
        let home = Arc::new(Home::new(images, registry, &dir)?);
        homes.insert(project.to_owned(), Arc::clone(&home));
        Ok(home)
    }

    /// Take project `project`'s home away, its image jobs cancelled, for its directory to be
    /// removed: `false`, and nothing taken, while a sandbox in it lives.
    pub fn close(&self, project: &str) -> bool {
        let mut homes = self.homes.lock().unwrap();
        if homes.get(project).is_some_and(|home| home.in_use()) {
            return false;
        }
        if let Some(home) = homes.remove(project) {
            home.close_all();
        }
        true
    }
}

/// The bearer token `headers` carry, if any.
pub fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
}

/// Who is a member of which project, kept in `members.json` in each project's directory: a
/// project has members while that file is there.
pub struct FileMembership {
    store: ProjectStore,
    /// Held while a project's members are read to be changed, and written.
    changing: Mutex<()>,
}

/// `members.json`, as kept.
#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Members {
    members: BTreeMap<String, Role>,
}

impl FileMembership {
    pub fn new(store: ProjectStore) -> FileMembership {
        FileMembership {
            store,
            changing: Mutex::default(),
        }
    }

    fn file(&self, project: &str) -> Result<PathBuf, ProjectError> {
        Ok(self.store.path(project)?.join("members.json"))
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

impl Membership for FileMembership {
    fn role<'a>(
        &'a self,
        user: &'a str,
        project: &'a str,
    ) -> BoxFuture<'a, Result<Option<Role>, ProjectError>> {
        Box::pin(async move {
            if !ProjectStore::is_name(project) {
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
            for name in self.store.list()? {
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
            let dir = self.store.path(project)?;
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

/// The users a set of tokens names, each token the user it names: for a test's requests, as
/// different users would send them.
#[cfg(test)]
pub(crate) struct TokenIsUser(pub std::collections::HashSet<String>);

#[cfg(test)]
impl Identity for TokenIsUser {
    fn identify<'a>(&'a self, token: Option<&'a str>) -> BoxFuture<'a, Option<String>> {
        Box::pin(async move {
            token
                .filter(|user| self.0.contains(*user))
                .map(str::to_owned)
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
    async fn the_config_s_projects_are_made_once_with_its_members_their_owners() {
        let dir = tempfile::tempdir().unwrap();
        let oidc = json!({ "issuer": "https://idp.example", "client_id": "virtx" });
        let configured = async |projects: serde_json::Value| {
            let config = json!({
                "listen": "127.0.0.1:0",
                "home": dir.path(),
                "oidc": oidc,
                "projects": projects,
            });
            Access::configured(&Config::parse(config).unwrap())
                .await
                .unwrap()
        };

        let access = configured(json!({
            "demo": { "members": ["alice@example.com", "bob@example.com"] },
            "nobody": {},
        }))
        .await;
        assert_eq!(
            access.membership().members("demo").await.unwrap(),
            [
                ("alice@example.com".to_owned(), Role::Owner),
                ("bob@example.com".to_owned(), Role::Owner)
            ]
        );
        // A project with no one to own it is not made.
        assert!(access.membership().members("nobody").await.is_err());

        // Once made, its members are the store's, whatever the config says after.
        let again = configured(json!({ "demo": { "members": ["mallory@example.com"] } })).await;
        assert_eq!(
            again
                .membership()
                .role("mallory@example.com", "demo")
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn a_member_reaches_their_project_and_none_other_and_default_by_no_name() {
        let dir = tempfile::tempdir().unwrap();
        let users = ["alice", "bob"].map(str::to_owned).into();
        let projects = Arc::new(FileMembership::new(ProjectStore::new(dir.path())));
        projects.create("demo", "alice").await.unwrap();
        projects.create(DEFAULT_PROJECT, "bob").await.unwrap();
        let access = Access::new(
            Vec::new(),
            Arc::new(TokenIsUser(users)),
            projects,
            ProjectStore::new(dir.path()),
        )
        .unwrap();
        let home = async |token, project| access.home(&headers(token, project, None)).await;

        assert_eq!(
            home(None, Some("demo")).await.err(),
            Some(StatusCode::UNAUTHORIZED)
        );
        assert_eq!(
            home(Some("mallory"), Some("demo")).await.err(),
            Some(StatusCode::UNAUTHORIZED)
        );
        assert_eq!(home(Some("alice"), Some("demo")).await.unwrap().0, "alice");
        assert_eq!(
            home(Some("bob"), Some("demo")).await.err(),
            Some(StatusCode::FORBIDDEN)
        );
        assert_eq!(
            home(Some("alice"), None).await.err(),
            Some(StatusCode::FORBIDDEN)
        );
        let (_, reached) = home(Some("bob"), None).await.unwrap();
        assert_eq!(
            reached.snapshots.root(),
            dir.path().join("project").join(DEFAULT_PROJECT).as_path()
        );
        assert_eq!(
            home(Some("alice"), Some("nowhere")).await.err(),
            Some(StatusCode::FORBIDDEN)
        );
        let elsewhere = headers(Some("alice"), Some("demo"), Some("https://evil.example"));
        assert_eq!(
            access.home(&elsewhere).await.err(),
            Some(StatusCode::FORBIDDEN)
        );
    }

    #[tokio::test]
    async fn members_reach_their_projects_in_their_roles_and_one_owner_stays() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path();
        let store = FileMembership::new(ProjectStore::new(home));

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
        let again = FileMembership::new(ProjectStore::new(home));
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
    }
}
