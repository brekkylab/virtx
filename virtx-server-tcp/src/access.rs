//! Who may send a request, and which project's [`Home`] it reaches. [`Access`] lets a request
//! in from where a browser may call, asks its [`Identity`] who the token is, asks its
//! [`Membership`] which project the request names and what they are in it, and takes that
//! project's home, made by its [`Homes`] as it is first reached.
//!
//! The server's own are [`Access::configured`]'s: the config's token issuer ([`Oidc`]), the
//! projects kept beside their directories ([`TcpMembership`]), and each home in its directory
//! ([`ProjectStore`]). A server built on this one gives its own of any, such as a membership
//! kept in a database.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::sync::{Arc, Mutex};

use futures_util::future::BoxFuture;
use http::{HeaderMap, StatusCode, header};
use virtx_core::{DEFAULT_PROJECT, PROJECT_HEADER};
use virtx_uvm::{ImageStore, ProjectError, ProjectId, ProjectStore, RegistryServer};

use crate::config::Config;
use crate::endpoints::Home;
use crate::oidc::Oidc;
use crate::project::{Membership, Owner, Project, TcpMembership};

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

/// What makes a project's home, as the project is first reached.
pub trait Homes: Send + Sync {
    /// Project `id`'s home: `None` where it is to have none, which no request then reaches.
    fn make<'a>(&'a self, id: &'a ProjectId) -> BoxFuture<'a, anyhow::Result<Option<Home>>>;
}

/// Each home in the project's directory, its images its own.
impl Homes for ProjectStore {
    fn make<'a>(&'a self, id: &'a ProjectId) -> BoxFuture<'a, anyhow::Result<Option<Home>>> {
        Box::pin(async move {
            let dir = self.path(id);
            let images = ImageStore::new(dir.join("images"))?;
            let registry = Arc::new(RegistryServer::new(images.clone(), REGISTRY_PATH)?);
            Ok(Some(Home::new(images, registry, &dir)?))
        })
    }
}

pub struct Access {
    /// The `Origin`s a browser may call from.
    origins: Vec<String>,
    identity: Arc<dyn Identity>,
    membership: Arc<dyn Membership>,
    /// Where the projects' directories are, each deleted as its project is removed.
    store: ProjectStore,
    maker: Arc<dyn Homes>,
    /// Each project's home, by id, made as it is first reached.
    /// `None` for a project being removed, which no request reaches until it is gone.
    homes: Mutex<HashMap<ProjectId, Option<Arc<Home>>>>,
    /// Held while a home is made, so that each is made once.
    making: tokio::sync::Mutex<()>,
}

impl Access {
    /// Requests from the `origins` a browser may call from, sent by whom `identity` says, to the
    /// projects `membership` names, kept in `store`, the home of each made by `maker` as it is
    /// first reached.
    pub fn new(
        origins: Vec<String>,
        identity: Arc<dyn Identity>,
        membership: Arc<dyn Membership>,
        store: ProjectStore,
        maker: Arc<dyn Homes>,
    ) -> Access {
        Access {
            origins,
            identity,
            membership,
            store,
            maker,
            homes: Mutex::default(),
            making: tokio::sync::Mutex::default(),
        }
    }

    /// What `config` says: the users of its issuer, the projects kept at its home, each home in
    /// its directory. The home of each project there is made now, so that its sandboxes left
    /// from before are swept as the server starts.
    pub async fn configured(config: &Config) -> anyhow::Result<Access> {
        let store = ProjectStore::new(config.home());
        let access = Access::new(
            config.origins.clone(),
            Arc::new(Oidc::new(config.oidc.clone())),
            Arc::new(TcpMembership::new(config.home())),
            store.clone(),
            Arc::new(store),
        );
        for id in access.store.list()? {
            access.home_of(&id).await?;
        }
        Ok(access)
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
    /// not exist, or has no home, which is not told apart.
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
        let home = self.home_of(&project.id).await.map_err(|e| {
            eprintln!("making project {}'s home: {e:#}", project.id);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
        Ok((user, home.ok_or(StatusCode::FORBIDDEN)?))
    }

    /// Project `id`'s home, made as it is first reached: `None` while it is being removed, or
    /// where it is to have none.
    async fn home_of(&self, id: &ProjectId) -> anyhow::Result<Option<Arc<Home>>> {
        if let Some(home) = self.homes.lock().unwrap().get(id) {
            return Ok(home.clone());
        }
        let _making = self.making.lock().await;
        if let Some(home) = self.homes.lock().unwrap().get(id) {
            return Ok(home.clone());
        }
        let Some(made) = self.maker.make(id).await? else {
            return Ok(None);
        };
        match self.homes.lock().unwrap().entry(*id) {
            // Taken away for its removal while it was made.
            Entry::Occupied(taken) => Ok(taken.get().clone()),
            Entry::Vacant(vacant) => Ok(vacant.insert(Some(Arc::new(made))).clone()),
        }
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
    use virtx_core::Role;

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
        let store = ProjectStore::new(dir.path());
        let access = Access::new(
            Vec::new(),
            Arc::new(TokenIsUser(users)),
            projects,
            store.clone(),
            Arc::new(store),
        );
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
        assert_eq!(reached.root, access.store().path(&demo));
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
        assert_eq!(own.root, access.store().path(&own_id));
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

    /// Homes made only for the projects listed, none for any other.
    struct Only(ProjectStore, Vec<ProjectId>);

    impl Homes for Only {
        fn make<'a>(&'a self, id: &'a ProjectId) -> BoxFuture<'a, anyhow::Result<Option<Home>>> {
            Box::pin(async move {
                if !self.1.contains(id) {
                    return Ok(None);
                }
                self.0.make(id).await
            })
        }
    }

    #[tokio::test]
    async fn a_project_its_homes_make_none_of_is_reached_by_no_request() {
        let dir = tempfile::tempdir().unwrap();
        let projects = Arc::new(TcpMembership::new(dir.path()));
        let alice = Owner::User("alice".into());
        let kept = projects.create(&alice, "kept", "alice").await.unwrap().id;
        let gone = projects.create(&alice, "gone", "alice").await.unwrap().id;
        let store = ProjectStore::new(dir.path());
        let access = Access::new(
            Vec::new(),
            Arc::new(TokenIsUser(["alice".to_owned()].into())),
            projects,
            store.clone(),
            Arc::new(Only(store.clone(), vec![kept])),
        );
        let home = async |project| {
            access
                .home(&headers(Some("alice"), Some(project), None))
                .await
        };

        // Made as first reached, and no sooner.
        assert!(access.homes().is_empty());
        let (_, reached) = home("kept").await.unwrap();
        assert!(Arc::ptr_eq(&home("kept").await.unwrap().1, &reached));
        assert_eq!(access.homes().len(), 1);
        assert_eq!(home("gone").await.err(), Some(StatusCode::FORBIDDEN));
        assert!(!store.path(&gone).join("images").exists());
    }
}
