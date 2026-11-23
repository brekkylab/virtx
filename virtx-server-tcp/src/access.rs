//! Who may send a request, and which project's [`Home`] it reaches. [`Access`] lets a request
//! in from where a browser may call, asks its [`Identity`] who the token is, asks its
//! [`Membership`] which project the request names and what they are in it, and takes that
//! project's home, made by its [`Homes`] as it is first reached.
//!
//! The server's own are [`Access::configured`]'s: the config's token issuer ([`Oidc`]), the
//! projects kept beside their directories ([`TcpMembership`]), each home in its directory
//! ([`ProjectStore`]), and the registries its image jobs reach ([`Config::registries`]). A
//! server built on this one gives its own of any, such as a membership kept in a database.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::sync::{Arc, Mutex};

use futures_util::future::BoxFuture;
use http::{HeaderMap, StatusCode, header};
use virtx_core::{DEFAULT_PROJECT, PROJECT_HEADER, Role};
use virtx_uvm::{
    ImageStore, ProjectError, ProjectId, ProjectStore, PublicImages, PublicRegistries,
    RegistryProxy, RegistryServer, Sandbox,
};

use crate::config::Config;
use crate::endpoints::Home;
use crate::oidc::Oidc;
use crate::project::{Membership, TcpMembership};

/// Where a home's images are served as a registry.
pub const REGISTRY_PATH: &str = "/images/oci";

/// Who a request is from, as its token was verified: its user, and the claims the token
/// carries, for a [`Membership`] to decide from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Principal {
    pub user: String,
    pub claims: serde_json::Map<String, serde_json::Value>,
}

impl Principal {
    /// `user`, with no claims.
    pub fn new(user: impl Into<String>) -> Principal {
        Principal {
            user: user.into(),
            claims: serde_json::Map::new(),
        }
    }
}

/// Who a server's tokens are from: it authenticates, and the [`Membership`] authorizes.
pub trait Identity: Send + Sync {
    /// Who a request carrying `token` is from: `None` for a token that is no one's, or for none
    /// where one is needed.
    fn identify<'a>(&'a self, token: Option<&'a str>) -> BoxFuture<'a, Option<Principal>>;
}

impl Identity for Oidc {
    fn identify<'a>(&'a self, token: Option<&'a str>) -> BoxFuture<'a, Option<Principal>> {
        Box::pin(async move { self.principal(token?).await })
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
            let made = move || Home::new(images, registry, &dir);
            Ok(Some(tokio::task::spawn_blocking(made).await??))
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
    /// `None` for a project being removed or removed, which no request reaches.
    homes: Mutex<HashMap<ProjectId, Option<Arc<Home>>>>,
    /// Held while a home is made, so that each is made once.
    making: tokio::sync::Mutex<()>,
    /// What the image jobs of the homes it makes connect to registries through.
    registries: Arc<dyn RegistryProxy>,
    /// Docker Hub's mirror, which the homes' anonymous pulls of Docker Hub's images go to first.
    mirror: Option<Arc<str>>,
    /// Where the homes' anonymous pulls are shared, in place of what each home was made with.
    public: Option<Arc<dyn PublicImages>>,
}

impl Access {
    /// Requests from the `origins` a browser may call from, sent by whom `identity` says, to the
    /// projects `membership` names, kept in `store`, the home of each made by `maker` as it is
    /// first reached. The homes' image jobs connect to registries on public addresses alone,
    /// through [`PublicRegistries`] with none named, unless [`Access::through`] says otherwise.
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
            registries: Arc::new(PublicRegistries::default()),
            mirror: None,
            public: None,
        }
    }

    /// This, the image jobs of the homes it makes from here on -- a pull, a push, a build's
    /// base -- connecting to registries through `registries`.
    pub fn through(mut self, registries: Arc<dyn RegistryProxy>) -> Access {
        self.registries = registries;
        self
    }

    /// This, the anonymous pulls of the homes it makes from here on -- a pull, a build's base --
    /// of Docker Hub's images going to its mirror at `mirror` first (`mirror.gcr.io`, say),
    /// through the registry proxy as any other connection. None is no mirror, as an access has
    /// by default. See [`RegistryClient::with_mirror`](virtx_uvm::RegistryClient::with_mirror).
    pub fn mirroring(mut self, mirror: Option<&str>) -> Access {
        self.mirror = mirror.map(Arc::from);
        self
    }

    /// This, the anonymous pulls of the homes it makes from here on shared through `public`, in
    /// place of what [`Homes`] made each with ([`Home::sharing`]). None leaves each as it was
    /// made.
    pub fn sharing(mut self, public: Option<Arc<dyn PublicImages>>) -> Access {
        self.public = public;
        self
    }

    /// What `config` says: the users of its issuer, the projects kept at its home, each home in
    /// its directory, the registries the homes' image jobs reach, Docker Hub's mirror, and,
    /// where `share_public_images` says so, the projects' anonymous pulls shared between them
    /// ([`SharedPulls`](crate::shared::SharedPulls)). The home of each project there is made now,
    /// so that its sandboxes left from before are swept as the server starts.
    pub async fn configured(config: &Config) -> anyhow::Result<Access> {
        let store = ProjectStore::new(config.home());
        let access = Access::new(
            config.origins.clone(),
            Arc::new(Oidc::new(config.oidc.clone())),
            Arc::new(TcpMembership::new(config.home())),
            store.clone(),
            Arc::new(store),
        )
        .through(Arc::new(config.registries()?))
        .mirroring(config.registry_mirror.as_deref())
        .sharing(config.public_images());
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

    /// Who a request with `headers` is from: `403` from an `Origin` not let in, and `401` for a
    /// token that is no one's.
    pub async fn principal(&self, headers: &HeaderMap) -> Result<Principal, StatusCode> {
        self.origin(headers)?;
        self.identity
            .identify(bearer(headers))
            .await
            .ok_or(StatusCode::UNAUTHORIZED)
    }

    /// The project `named` names to `who`, and their role in it, if they have one: their own,
    /// [`DEFAULT_PROJECT`], where it is `None`; the project of that id, where it is one; and
    /// otherwise the one the [`Membership`] resolves the name to. The membership decides both.
    pub async fn project(
        &self,
        who: &Principal,
        named: Option<&str>,
    ) -> Result<Option<(ProjectId, Role)>, ProjectError> {
        let named = named.unwrap_or(DEFAULT_PROJECT);
        let id = match named.parse::<ProjectId>() {
            Ok(id) => id,
            Err(_) => match self.membership.resolve(who, named).await? {
                Some(id) => id,
                None => return Ok(None),
            },
        };
        Ok(self.membership.role(&id, who).await?.map(|role| (id, role)))
    }

    /// Who a request with `headers` is from, and the home of the project it names, as
    /// [`project`](Access::project) reads it: as [`principal`](Access::principal) refuses, `400`
    /// for a project that is no string, and `403` for one its sender has no role in -- or that
    /// does not exist, or has no home, which is not told apart.
    pub async fn home(&self, headers: &HeaderMap) -> Result<(Principal, Arc<Home>), StatusCode> {
        let who = self.principal(headers).await?;
        let named = match headers.get(PROJECT_HEADER) {
            Some(named) => Some(named.to_str().map_err(|_| StatusCode::BAD_REQUEST)?),
            None => None,
        };
        let (id, _) = self
            .project(&who, named)
            .await
            .map_err(|e| {
                eprintln!("reaching {}'s project {named:?}: {e:#}", who.user);
                StatusCode::INTERNAL_SERVER_ERROR
            })?
            .ok_or(StatusCode::FORBIDDEN)?;
        let home = self.home_of(&id).await.map_err(|e| {
            eprintln!("making project {id}'s home: {e:#}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
        Ok((who, home.ok_or(StatusCode::FORBIDDEN)?))
    }

    /// Project `id`'s home, made as it is first reached, by a request or by this: `None` once it
    /// is being removed, or where it is to have none. It asks no one whether `id` is a project's:
    /// that is the caller's to know, or its [`Homes`]' to refuse. A `Homes` that makes a home for
    /// any id, as `ProjectStore` does, makes one, and its directory, for an id no project has.
    pub async fn home_of(&self, id: &ProjectId) -> anyhow::Result<Option<Arc<Home>>> {
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
        let mut made = made.through(Arc::clone(&self.registries), self.mirror.clone());
        if let Some(public) = &self.public {
            made = made.sharing(Some(Arc::clone(public)));
        }
        match self.homes.lock().unwrap().entry(*id) {
            // Taken away for its removal while it was made.
            Entry::Occupied(taken) => Ok(taken.get().clone()),
            Entry::Vacant(vacant) => Ok(vacant.insert(Some(Arc::new(made))).clone()),
        }
    }

    /// Take project `id`'s home away, closed and its image jobs cancelled, for it to be removed:
    /// no request reaches it from now on, or until [`release`](Access::release) where the
    /// removal fails. `false`, and nothing taken, while a sandbox lives or is being made in it,
    /// or it is being removed already.
    pub fn close(&self, id: &ProjectId) -> bool {
        let mut homes = self.homes.lock().unwrap();
        match homes.get(id) {
            Some(None) => return false,
            Some(Some(home)) if !home.close() => return false,
            None if Sandbox::lives_under(&self.store.path(id)) => return false,
            _ => {}
        }
        if let Some(Some(home)) = homes.insert(*id, None) {
            home.close_all();
        }
        true
    }

    /// Let requests reach project `id` again, once its removal has failed.
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
    fn identify<'a>(&'a self, token: Option<&'a str>) -> BoxFuture<'a, Option<Principal>> {
        Box::pin(async move {
            token
                .filter(|user| self.0.contains(*user))
                .map(Principal::new)
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

    /// The registry proxy of the access a config makes lets a job through to a registry on this
    /// host's loopback that its `private_registries` names, and refuses one beside it.
    #[tokio::test]
    async fn a_config_s_private_registries_are_let_through_its_registry_proxy() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let dir = tempfile::tempdir().unwrap();
        let listeners = [(); 2].map(|_| std::net::TcpListener::bind("127.0.0.1:0").unwrap());
        let [named, other] = listeners
            .each_ref()
            .map(|listener| listener.local_addr().unwrap().to_string());
        let config = Config::parse(json!({
            "listen": "127.0.0.1:0",
            "home": dir.path(),
            "oidc": { "issuer": "https://idp.example", "client_id": "virtx" },
            "private_registries": [named],
        }))
        .unwrap();
        let access = Access::configured(&config).await.unwrap();
        let pass = access.registries.pass();
        let answered = async |host: &str| {
            let at = pass.url().strip_prefix("http://").unwrap();
            let mut stream = tokio::net::TcpStream::connect(at).await.unwrap();
            let request = format!("CONNECT {host} HTTP/1.1\r\nHost: {host}\r\n\r\n");
            stream.write_all(request.as_bytes()).await.unwrap();
            let mut status = [0; 12];
            stream.read_exact(&mut status).await.unwrap();
            String::from_utf8(status.to_vec()).unwrap()
        };
        assert_eq!(answered(&named).await, "HTTP/1.1 200");
        assert_eq!(answered(&other).await, "HTTP/1.1 403");
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
        let alice_is = Principal::new("alice@example.com");
        let demo = projects.create(&alice_is, "demo").await.unwrap().id;
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
        let (who, reached) = home(alice, Some("demo")).await.unwrap();
        assert_eq!(who, alice_is);
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
            .resolve(&alice_is, DEFAULT_PROJECT)
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

    /// A project is closed for its removal only while no sandbox of this process or another lives
    /// in it; closed, a request already in its home starts nothing, and removed, it gets no home
    /// again.
    #[tokio::test]
    async fn a_project_is_closed_only_while_no_sandbox_lives_in_it() {
        let dir = tempfile::tempdir().unwrap();
        let projects = Arc::new(TcpMembership::new(dir.path()));
        let alice = Principal::new("alice");
        let demo = projects.create(&alice, "demo").await.unwrap().id;
        let unreached = projects.create(&alice, "unreached").await.unwrap().id;
        let store = ProjectStore::new(dir.path());
        let access = Access::new(
            Vec::new(),
            Arc::new(TokenIsUser(["alice".to_owned()].into())),
            projects,
            store.clone(),
            Arc::new(store.clone()),
        );
        let (_, home) = access
            .home(&headers(Some("alice"), Some("demo"), None))
            .await
            .unwrap();

        // An upper disk another process's sandbox holds, in a project with a home here or not.
        for id in [demo, unreached] {
            let sandbox = store.path(&id).join("sandbox");
            std::fs::create_dir_all(&sandbox).unwrap();
            let held = std::fs::File::create(sandbox.join("other.ext4.lock")).unwrap();
            held.lock().unwrap();
            assert!(!access.close(&id));
            drop(held);
        }

        assert!(access.close(&demo));
        let pull = http::Request::post("/images/jobs/pull/")
            .header(header::CONTENT_TYPE, "application/json")
            .body(axum::body::Body::from(r#"{"reference": "alpine:3.20"}"#))
            .unwrap();
        let in_home = crate::endpoints::router().layer(axum::Extension(home));
        let Ok(refused) = tower::ServiceExt::oneshot(in_home, pull).await;
        assert_eq!(refused.status(), StatusCode::FORBIDDEN);
        assert!(!access.close(&demo));
        assert!(access.home_of(&demo).await.unwrap().is_none());
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
        let alice = Principal::new("alice");
        let kept = projects.create(&alice, "kept").await.unwrap().id;
        let gone = projects.create(&alice, "gone").await.unwrap().id;
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

    /// A project's home is reached by its id as a request reaches it, made by whichever comes
    /// first, and none for a project being removed or one its homes make none of.
    #[tokio::test]
    async fn a_project_s_home_is_reached_by_its_id_as_by_a_request() {
        let dir = tempfile::tempdir().unwrap();
        let projects = Arc::new(TcpMembership::new(dir.path()));
        let alice = Principal::new("alice");
        let first = projects.create(&alice, "first").await.unwrap().id;
        let second = projects.create(&alice, "second").await.unwrap().id;
        let none = projects.create(&alice, "none").await.unwrap().id;
        let store = ProjectStore::new(dir.path());
        let access = Access::new(
            Vec::new(),
            Arc::new(TokenIsUser(["alice".to_owned()].into())),
            projects,
            store.clone(),
            Arc::new(Only(store.clone(), vec![first, second])),
        );
        let requested = async |project| {
            let headers = headers(Some("alice"), Some(project), None);
            access.home(&headers).await.unwrap().1
        };

        // By its id first, then by a request: the one home.
        let home = access.home_of(&first).await.unwrap().unwrap();
        assert_eq!(home.root, store.path(&first));
        assert!(Arc::ptr_eq(&requested("first").await, &home));
        // By a request first, then by its id: the one home.
        let home = requested("second").await;
        let by_id = access.home_of(&second).await.unwrap().unwrap();
        assert!(Arc::ptr_eq(&by_id, &home));

        assert!(access.home_of(&none).await.unwrap().is_none());
        assert!(access.close(&first));
        assert!(access.home_of(&first).await.unwrap().is_none());
    }
}
