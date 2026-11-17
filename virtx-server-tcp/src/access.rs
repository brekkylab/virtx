//! Who may send a request, and which project's [`Home`] it reaches. [`Access`] lets a request
//! in from where a browser may call, asks its [`Identity`] who the token is, asks its
//! [`Membership`] which project the request names and what they are in it, and takes that
//! project's home, made as it is first reached.
//!
//! The server's own are [`Access::configured`]'s: the config's OpenID Connect provider
//! ([`Oidc`]), and the projects kept beside their directories ([`TcpMembership`]). A server
//! built on this one gives its own of either, such as a membership kept in a database.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use futures_util::future::BoxFuture;
use http::{HeaderMap, StatusCode, header};
use virtx_core::{DEFAULT_PROJECT, PROJECT_HEADER, Role};
use virtx_uvm::{
    ImageStore, Membership, Owner, Project, ProjectError, ProjectId, ProjectStore, RegistryServer,
};

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
    /// Each project's home, by id, made as it is first reached.
    /// `None` for a project being removed, which no request reaches until it is gone.
    homes: Mutex<HashMap<ProjectId, Option<Arc<Home>>>>,
}

impl Access {
    /// Requests from the `origins` a browser may call from, sent by whom `identity` says, to the
    /// projects `membership` names, kept in `store`. The home of each project in `store` is made
    /// now, so that its sandboxes left from before are swept as the server starts.
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
        for id in access.store.list()? {
            access.home_of(&id)?;
        }
        Ok(access)
    }

    /// What `config` says: the users of its provider, the projects kept at its home.
    pub fn configured(config: &Config) -> anyhow::Result<Access> {
        Access::new(
            config.origins.clone(),
            Arc::new(Oidc::new(config.oidc.clone())),
            Arc::new(TcpMembership::new(config.home())),
            ProjectStore::new(config.home()),
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
        self.homes
            .lock()
            .unwrap()
            .values()
            .flatten()
            .cloned()
            .collect()
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

    /// The project `named` names to `user`, if they are a member of it: their own,
    /// [`DEFAULT_PROJECT`], where it is `None`; the project of that id, where it is one; and
    /// otherwise the project of theirs of that name.
    pub async fn project(
        &self,
        user: &str,
        named: Option<&str>,
    ) -> Result<Option<Project>, ProjectError> {
        let named = named.unwrap_or(DEFAULT_PROJECT);
        let id = match named.parse::<ProjectId>() {
            Ok(id) => id,
            Err(_) => {
                let owner = Owner::User(user.to_owned());
                match self.membership.resolve(&owner, named).await? {
                    Some(id) => id,
                    None => return Ok(None),
                }
            }
        };
        let project = self.membership.project(&id).await?;
        Ok(project.filter(|project| project.members.contains_key(user)))
    }

    /// Who a request with `headers` is sent by, and the home of the project it names, as
    /// [`project`](Access::project) reads it: as [`user`](Access::user) refuses, `400` for a
    /// project that is no string, and `403` for one its sender is no member of -- or that does
    /// not exist, which is not told apart.
    pub async fn home(&self, headers: &HeaderMap) -> Result<(String, Arc<Home>), StatusCode> {
        let user = self.user(headers).await?;
        let named = match headers.get(PROJECT_HEADER) {
            Some(named) => Some(named.to_str().map_err(|_| StatusCode::BAD_REQUEST)?),
            None => None,
        };
        let project = self
            .project(&user, named)
            .await
            .map_err(|e| {
                eprintln!("reaching {user}'s project {named:?}: {e}");
                StatusCode::INTERNAL_SERVER_ERROR
            })?
            .ok_or(StatusCode::FORBIDDEN)?;
        let home = self.home_of(&project.id).map_err(|e| {
            eprintln!("making project {}'s home: {e:#}", project.id);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
        Ok((user, home.ok_or(StatusCode::FORBIDDEN)?))
    }

    /// Project `id`'s home, its images its own: `None` while it is being removed.
    fn home_of(&self, id: &ProjectId) -> anyhow::Result<Option<Arc<Home>>> {
        let mut homes = self.homes.lock().unwrap();
        if let Some(home) = homes.get(id) {
            return Ok(home.clone());
        }
        let dir = self.store.path(id);
        let images = ImageStore::new(dir.join("images"))?;
        let registry = Arc::new(RegistryServer::new(images.clone(), REGISTRY_PATH)?);
        let home = Arc::new(Home::new(images, registry, &dir)?);
        homes.insert(*id, Some(Arc::clone(&home)));
        Ok(Some(home))
    }

    /// Take project `id`'s home away, its image jobs cancelled, for it to be removed: no
    /// request reaches it from now until [`release`](Access::release). `false`, and nothing
    /// taken, while a sandbox in it lives or it is being removed already.
    pub fn close(&self, id: &ProjectId) -> bool {
        let mut homes = self.homes.lock().unwrap();
        match homes.get(id) {
            Some(None) => return false,
            Some(Some(home)) if home.in_use() => return false,
            _ => {}
        }
        if let Some(Some(home)) = homes.insert(*id, None) {
            home.close_all();
        }
        true
    }

    /// Let requests reach project `id` again, once its removal is over.
    pub fn release(&self, id: &ProjectId) {
        self.homes.lock().unwrap().remove(id);
    }
}

/// The bearer token `headers` carry, if any.
pub fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
}

/// The projects, each kept in its directory as `project.json`, the [`Project`] with its
/// members: a project is there while that file is. Each change is made holding `project/.lock`,
/// so that servers over one root change them one at a time.
pub struct TcpMembership {
    store: ProjectStore,
}

impl TcpMembership {
    /// The projects kept under `root`.
    pub fn new(root: impl Into<PathBuf>) -> TcpMembership {
        TcpMembership {
            store: ProjectStore::new(root),
        }
    }

    /// Project `id`, as kept: `None` where it is not there.
    fn read(&self, id: &ProjectId) -> Result<Option<Project>, ProjectError> {
        let path = self.store.path(id).join("project.json");
        match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|e| ProjectError::io(format!("reading {}", path.display()), e.into())),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(ProjectError::io(format!("reading {}", path.display()), e)),
        }
    }

    /// Keep `project` whole: written beside, and renamed over.
    fn write(&self, project: &Project) -> Result<(), ProjectError> {
        let dir = self.store.path(&project.id);
        fs::create_dir_all(&dir)
            .map_err(|e| ProjectError::io(format!("making {}", dir.display()), e))?;
        let (path, part) = (dir.join("project.json"), dir.join("project.json.part"));
        let bytes = serde_json::to_vec_pretty(project).expect("JSON of strings");
        fs::write(&part, bytes)
            .map_err(|e| ProjectError::io(format!("writing {}", part.display()), e))?;
        fs::rename(&part, &path)
            .map_err(|e| ProjectError::io(format!("writing {}", path.display()), e))
    }

    /// Every project here.
    fn all(&self) -> Result<Vec<Project>, ProjectError> {
        let mut all = Vec::new();
        for id in self.store.list()? {
            all.extend(self.read(&id)?);
        }
        Ok(all)
    }

    /// The project `owner` names `name`, if any, as kept.
    fn find(&self, owner: &Owner, name: &str) -> Result<Option<Project>, ProjectError> {
        let all = self.all()?;
        Ok(all
            .into_iter()
            .find(|project| project.owner == *owner && project.name == name))
    }

    /// The lock held while the projects change, until it drops.
    fn lock(&self) -> Result<fs::File, ProjectError> {
        let dir = self.store.dir();
        fs::create_dir_all(&dir)
            .map_err(|e| ProjectError::io(format!("making {}", dir.display()), e))?;
        let path = dir.join(".lock");
        let lock = fs::File::create(&path)
            .map_err(|e| ProjectError::io(format!("opening {}", path.display()), e))?;
        lock.lock()
            .map_err(|e| ProjectError::io(format!("locking {}", path.display()), e))?;
        Ok(lock)
    }

    /// Make project `name` of `owner`'s, `creator` its owner, holding the lock.
    fn make(&self, owner: &Owner, name: &str, creator: &str) -> Result<Project, ProjectError> {
        let _lock = self.lock()?;
        if self.find(owner, name)?.is_some() {
            return Err(ProjectError::AlreadyExists(name.to_owned()));
        }
        let project = Project {
            id: ProjectId::new(),
            owner: owner.clone(),
            name: name.to_owned(),
            members: BTreeMap::from([(creator.to_owned(), Role::Owner)]),
        };
        self.write(&project)?;
        Ok(project)
    }
}

impl Membership for TcpMembership {
    fn resolve<'a>(
        &'a self,
        owner: &'a Owner,
        name: &'a str,
    ) -> BoxFuture<'a, Result<Option<ProjectId>, ProjectError>> {
        Box::pin(async move {
            if let Some(project) = self.find(owner, name)? {
                return Ok(Some(project.id));
            }
            if name != DEFAULT_PROJECT {
                return Ok(None);
            }
            let Owner::User(user) = owner else {
                return Ok(None);
            };
            match self.make(owner, name, user) {
                Ok(project) => Ok(Some(project.id)),
                // Made by a request beside this one.
                Err(ProjectError::AlreadyExists(_)) => Ok(self.find(owner, name)?.map(|p| p.id)),
                Err(e) => Err(e),
            }
        })
    }

    fn project<'a>(
        &'a self,
        id: &'a ProjectId,
    ) -> BoxFuture<'a, Result<Option<Project>, ProjectError>> {
        Box::pin(async move { self.read(id) })
    }

    fn create<'a>(
        &'a self,
        owner: &'a Owner,
        name: &'a str,
        creator: &'a str,
    ) -> BoxFuture<'a, Result<Project, ProjectError>> {
        Box::pin(async move {
            if !virtx_uvm::is_project_name(name) {
                return Err(ProjectError::InvalidName(name.to_owned()));
            }
            self.make(owner, name, creator)
        })
    }

    fn projects<'a>(&'a self, user: &'a str) -> BoxFuture<'a, Result<Vec<Project>, ProjectError>> {
        Box::pin(async move {
            let mut reached = self.all()?;
            reached.retain(|project| project.members.contains_key(user));
            reached.sort_by(|a, b| (&a.name, &a.owner).cmp(&(&b.name, &b.owner)));
            Ok(reached)
        })
    }

    fn set_member<'a>(
        &'a self,
        id: &'a ProjectId,
        user: &'a str,
        role: Option<Role>,
    ) -> BoxFuture<'a, Result<(), ProjectError>> {
        Box::pin(async move {
            let _lock = self.lock()?;
            let mut project = self
                .read(id)?
                .ok_or_else(|| ProjectError::Unknown(id.to_string()))?;
            if project.owner == Owner::User(user.to_owned()) && role != Some(Role::Owner) {
                return Err(ProjectError::NoOwner(id.to_string()));
            }
            match role {
                Some(role) => project.members.insert(user.to_owned(), role),
                None => project.members.remove(user),
            };
            self.write(&project)
        })
    }

    fn remove<'a>(&'a self, id: &'a ProjectId) -> BoxFuture<'a, Result<(), ProjectError>> {
        Box::pin(async move {
            let _lock = self.lock()?;
            let path = self.store.path(id).join("project.json");
            match fs::remove_file(&path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    Err(ProjectError::Unknown(id.to_string()))
                }
                Err(e) => Err(ProjectError::io(format!("removing {}", path.display()), e)),
            }
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
    use virtx_core::{DEFAULT_PROJECT, Role};
    use virtx_uvm::Owner;

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
    fn a_config_names_its_provider() {
        let refused = Config::parse(json!({ "listen": "127.0.0.1:0" }))
            .err()
            .unwrap();
        assert!(refused.to_string().contains("oidc"), "{refused}");
    }

    #[tokio::test]
    async fn a_member_reaches_their_project_and_none_other_and_their_own_by_no_name() {
        let dir = tempfile::tempdir().unwrap();
        let users = [
            "alice@example.com",
            "bob@example.com",
            "a+b@x.com",
            "a-b@x.com",
        ]
        .map(str::to_owned)
        .into();
        let projects = Arc::new(TcpMembership::new(dir.path()));
        let owner = Owner::User("alice@example.com".into());
        let demo = projects
            .create(&owner, "demo", "alice@example.com")
            .await
            .unwrap()
            .id;
        let access = Access::new(
            Vec::new(),
            Arc::new(TokenIsUser(users)),
            projects,
            ProjectStore::new(dir.path()),
        )
        .unwrap();
        let home = async |token, project| access.home(&headers(token, project, None)).await;
        let (alice, bob) = (Some("alice@example.com"), Some("bob@example.com"));
        let id = demo.to_string();

        assert_eq!(
            home(None, Some("demo")).await.err(),
            Some(StatusCode::UNAUTHORIZED)
        );
        assert_eq!(
            home(Some("mallory"), Some("demo")).await.err(),
            Some(StatusCode::UNAUTHORIZED)
        );
        let (user, reached) = home(alice, Some("demo")).await.unwrap();
        assert_eq!(user, "alice@example.com");
        assert_eq!(reached.snapshots.root(), access.store().path(&demo));
        // By its id, the same home.
        assert!(Arc::ptr_eq(
            &home(alice, Some(&id)).await.unwrap().1,
            &reached
        ));
        assert_eq!(
            home(bob, Some(&id)).await.err(),
            Some(StatusCode::FORBIDDEN)
        );
        assert_eq!(
            home(alice, Some("nowhere")).await.err(),
            Some(StatusCode::FORBIDDEN)
        );
        let elsewhere = headers(alice, Some("demo"), Some("https://evil.example"));
        assert_eq!(
            access.home(&elsewhere).await.err(),
            Some(StatusCode::FORBIDDEN)
        );

        // Naming none, each reaches a project of their own, made as first reached, theirs alone.
        let (_, own) = home(alice, None).await.unwrap();
        let own_id = access
            .membership()
            .resolve(&owner, DEFAULT_PROJECT)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(own.snapshots.root(), access.store().path(&own_id));
        assert!(Arc::ptr_eq(
            &home(alice, Some(DEFAULT_PROJECT)).await.unwrap().1,
            &own
        ));
        let kept = access.membership().project(&own_id).await.unwrap().unwrap();
        assert_eq!(
            (
                kept.name.as_str(),
                kept.members.into_iter().collect::<Vec<_>>()
            ),
            (
                DEFAULT_PROJECT,
                vec![("alice@example.com".to_owned(), Role::Owner)]
            )
        );
        let own_id = own_id.to_string();
        assert_eq!(
            home(bob, Some(&own_id)).await.err(),
            Some(StatusCode::FORBIDDEN)
        );
        let (_, theirs) = home(bob, None).await.unwrap();
        assert!(!Arc::ptr_eq(&theirs, &own));
        // Users whose names differ only in what a name has not have each their own.
        let (_, plus) = home(Some("a+b@x.com"), None).await.unwrap();
        let (_, minus) = home(Some("a-b@x.com"), None).await.unwrap();
        assert!(!Arc::ptr_eq(&plus, &minus));
    }

    #[tokio::test]
    async fn an_owner_names_each_project_once_and_its_members_reach_it_in_their_roles() {
        let root = tempfile::tempdir().unwrap();
        let store = TcpMembership::new(root.path());
        let (alice, bob) = (Owner::User("alice".into()), Owner::User("bob".into()));

        assert_eq!(store.resolve(&alice, "demo").await.unwrap(), None);
        let demo = store.create(&alice, "demo", "alice").await.unwrap();
        assert!(
            root.path()
                .join("project")
                .join(demo.id.to_string())
                .join("project.json")
                .is_file()
        );
        assert_eq!(store.resolve(&alice, "demo").await.unwrap(), Some(demo.id));
        assert!(matches!(
            store.create(&alice, "demo", "alice").await,
            Err(ProjectError::AlreadyExists(_))
        ));
        assert!(matches!(
            store.create(&alice, "Demo", "alice").await,
            Err(ProjectError::InvalidName(_))
        ));
        // Another owner's of the same name is another project.
        let theirs = store.create(&bob, "demo", "bob").await.unwrap();
        assert_ne!(theirs.id, demo.id);
        assert_eq!(store.resolve(&bob, "demo").await.unwrap(), Some(theirs.id));
        assert_eq!(store.project(&demo.id).await.unwrap(), Some(demo.clone()));

        store
            .set_member(&demo.id, "bob", Some(Role::Member))
            .await
            .unwrap();
        let listed: Vec<_> = store
            .projects("bob")
            .await
            .unwrap()
            .into_iter()
            .map(|project| (project.owner, project.members["bob"]))
            .collect();
        assert_eq!(listed, [(alice.clone(), Role::Member), (bob, Role::Owner)]);

        // Its owner neither leaves nor steps down; another owner leaves.
        for role in [None, Some(Role::Member)] {
            assert!(matches!(
                store.set_member(&demo.id, "alice", role).await,
                Err(ProjectError::NoOwner(_))
            ));
        }
        store
            .set_member(&demo.id, "bob", Some(Role::Owner))
            .await
            .unwrap();
        store.set_member(&demo.id, "bob", None).await.unwrap();

        // Kept on disk, for the next membership over the same root.
        let again = TcpMembership::new(root.path());
        let kept = again.project(&demo.id).await.unwrap().unwrap();
        assert_eq!(
            kept.members.into_iter().collect::<Vec<_>>(),
            [("alice".to_owned(), Role::Owner)]
        );

        again.remove(&demo.id).await.unwrap();
        assert_eq!(again.project(&demo.id).await.unwrap(), None);
        assert_eq!(again.resolve(&alice, "demo").await.unwrap(), None);
        assert!(matches!(
            again.remove(&demo.id).await,
            Err(ProjectError::Unknown(_))
        ));
        // Its name is its owner's to give again, to a project of another id.
        let anew = again.create(&alice, "demo", "alice").await.unwrap();
        assert_ne!(anew.id, demo.id);
    }
}
