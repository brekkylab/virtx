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
use http::{HeaderMap, Method, StatusCode, header};
use virtx_core::{DEFAULT_PROJECT, PROJECT_HEADER, Preset, PresetAccess, Role};
use virtx_uvm::{
    ImageStore, ProjectError, ProjectId, ProjectStore, PublicImages, PublicRegistries,
    RegistryProxy, RegistryServer, Sandbox,
};

use crate::config::Config;
use crate::endpoints::{Home, Maker, Refused, refusal, refused};
use crate::grant::{self, Grant};
use crate::oidc::{Issuer, Oidc};
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
            let project = *id;
            let made = move || Home::new(project, images, registry, &dir);
            Ok(Some(tokio::task::spawn_blocking(made).await??))
        })
    }
}

/// An issuer, and where its keys are where it is said.
type IssuerKeys = (String, Option<String>);

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
    /// The issuers presets' access names, by issuer and where its keys are, each made as a
    /// token is first checked against it.
    issuers: Mutex<HashMap<IssuerKeys, Arc<Issuer>>>,
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
            issuers: Mutex::default(),
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
        self.origin_among(headers, &[])
    }

    /// `403` for a request with `headers` from an `Origin` neither the server lets in nor one
    /// of `also`.
    fn origin_among(&self, headers: &HeaderMap, also: &[String]) -> Result<(), StatusCode> {
        if let Some(origin) = headers.get(header::ORIGIN)
            && !(self.origins.iter().chain(also))
                .any(|allowed| allowed.as_bytes() == origin.as_bytes())
        {
            return Err(StatusCode::FORBIDDEN);
        }
        Ok(())
    }

    /// The home a request to `path` with `method` and `headers` reaches, and its grant where it
    /// is sent with one: a grant's own project's, as far as [`Grant::reaches`] says, `401` for
    /// one no longer given, and `403` for anything else of it, from an origin neither the
    /// server nor its preset let in, or naming another project; otherwise a member's, as
    /// [`home`](Access::home) says.
    pub async fn reach(
        &self,
        method: &Method,
        path: &str,
        headers: &HeaderMap,
    ) -> Result<(Arc<Home>, Option<Grant>), StatusCode> {
        let Some(project) = bearer(headers).and_then(grant::project_of) else {
            let (_, home) = self.home(headers).await?;
            return Ok((home, None));
        };
        let home = self.home_of(&project).await.map_err(|e| {
            eprintln!("making project {project}'s home: {e:#}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
        let home = home.ok_or(StatusCode::UNAUTHORIZED)?;
        let grant = home
            .grant(bearer(headers).unwrap_or_default())
            .await
            .map_err(|refused| refused.status())?
            .ok_or(StatusCode::UNAUTHORIZED)?;
        self.origin_among(headers, &grant.origins)?;
        let named = headers.get(PROJECT_HEADER);
        if named.is_some_and(|named| named.as_bytes() != project.to_string().as_bytes())
            || !grant.reaches(method, path)
        {
            return Err(StatusCode::FORBIDDEN);
        }
        Ok((home, Some(grant)))
    }

    /// The home of the preset `named` names, the preset, and who makes a sandbox of it with
    /// `headers`, from `from`: a member of its project, with their token, or one its access
    /// lets in. Named by its id, it is reached with no member's token; by its name, in the
    /// project the request names, by members alone. Refused `404` for no such preset, `401`
    /// for a request no way lets in with no credentials or a token no one's, `403` for one
    /// with a token that is someone's but not one the preset takes, or a grant, and from an
    /// origin neither the server nor the preset lets in.
    pub(crate) async fn maker(
        &self,
        headers: &HeaderMap,
        named: &str,
        from: Option<std::net::IpAddr>,
    ) -> Result<(Arc<Home>, Preset, Maker), Refused> {
        let token = bearer(headers);
        if token.and_then(grant::project_of).is_some() {
            return Err(refusal(StatusCode::FORBIDDEN));
        }
        let none = || {
            refused(
                StatusCode::NOT_FOUND,
                "not_found",
                format!("no preset here is {named}"),
            )
        };
        let by_id =
            (named.split_once('.')).and_then(|(project, _)| project.parse::<ProjectId>().ok());
        let (home, who, member) = match by_id {
            Some(project) => {
                let home = self.home_of(&project).await.map_err(|e| {
                    eprintln!("making project {project}'s home: {e:#}");
                    refusal(StatusCode::INTERNAL_SERVER_ERROR)
                })?;
                let home = home.ok_or_else(none)?;
                let who = self.identity.identify(token).await;
                let member = match &who {
                    Some(who) => self.membership.role(&project, who).await?.is_some(),
                    None => false,
                };
                (home, who, member)
            }
            None => {
                let (who, home) = self.home(headers).await.map_err(refusal)?;
                (home, Some(who), true)
            }
        };
        let preset = home.preset(named).await?;
        self.origin_among(headers, &preset.origins)
            .map_err(refusal)?;
        if member && let Some(who) = who {
            return Ok((home, preset, Maker::Member(who.user)));
        }
        let mut someone_s = who.is_some();
        for way in &preset.access {
            match (way, token) {
                (PresetAccess::Public, None) => {
                    let subject =
                        from.map_or_else(|| "unknown".to_owned(), |from| from.to_string());
                    return Ok((home, preset, Maker::LetIn(subject)));
                }
                (
                    PresetAccess::Jwt {
                        issuer,
                        audience,
                        jwks,
                        claims,
                    },
                    Some(token),
                ) => {
                    let issuer = self.issuer(issuer, jwks.as_deref());
                    let Some(carried) = issuer.claims(token, audience).await else {
                        continue;
                    };
                    someone_s = true;
                    let subject = carried.get("sub").and_then(|sub| sub.as_str());
                    if let Some(subject) = subject
                        && claims
                            .iter()
                            .all(|(name, value)| carried.get(name) == Some(value))
                    {
                        return Ok((home, preset, Maker::LetIn(subject.to_owned())));
                    }
                }
                _ => {}
            }
        }
        Err(refusal(match someone_s {
            true => StatusCode::FORBIDDEN,
            false => StatusCode::UNAUTHORIZED,
        }))
    }

    /// The issuer `issuer`, its keys at `jwks` where it says, made as it is first asked for.
    fn issuer(&self, issuer: &str, jwks: Option<&str>) -> Arc<Issuer> {
        let key = (issuer.to_owned(), jwks.map(str::to_owned));
        let mut issuers = self.issuers.lock().unwrap();
        Arc::clone(
            issuers.entry(key).or_insert_with(|| {
                Arc::new(Issuer::new(issuer.to_owned(), jwks.map(str::to_owned)))
            }),
        )
    }

    /// Who a request with `headers` is from: `403` from an `Origin` not let in, and for a
    /// [grant](crate::grant), which is no one; `401` for a token that is no one's.
    pub async fn principal(&self, headers: &HeaderMap) -> Result<Principal, StatusCode> {
        self.origin(headers)?;
        if bearer(headers).and_then(grant::project_of).is_some() {
            return Err(StatusCode::FORBIDDEN);
        }
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

    /// Take project `id`'s home away for good, whatever lives in it, for the caller to delete
    /// its directory: no request reaches it from now on, one already in it starts no sandbox or
    /// image job, and every image job in it is cancelled and every sandbox in it closed, as
    /// [`Home::close_all`] does. One kept there and not known again ([`UnknownSandbox`]) is let
    /// go of, its disk held by this process no more. Blocks until each machine has stopped. Left
    /// to the caller: the directory and what else is in it -- its images and volumes, and the
    /// disk and record of each sandbox let go of -- what a backend keeps of it, and a sandbox
    /// another process holds there ([`Sandbox::lives_under`]).
    /// [`release`](Access::release) lets requests reach it again.
    ///
    /// [`UnknownSandbox`]: virtx_uvm::UnknownSandbox
    pub fn forget(&self, id: &ProjectId) {
        let taken = self.homes.lock().unwrap().insert(*id, None).flatten();
        if let Some(home) = taken {
            home.forget();
        }
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

    /// A project is forgotten whatever its home holds: a sandbox kept in it and not known again,
    /// which keeps it from being closed, is let go of, its files left for the caller, and its
    /// home leaves the homes made and is reached by no request again.
    #[tokio::test]
    async fn a_project_is_forgotten_with_a_sandbox_not_known_again_in_it() {
        let dir = tempfile::tempdir().unwrap();
        let projects = Arc::new(TcpMembership::new(dir.path()));
        let alice = Principal::new("alice");
        let demo = projects.create(&alice, "demo").await.unwrap().id;
        let store = ProjectStore::new(dir.path());
        let access = Access::new(
            Vec::new(),
            Arc::new(TokenIsUser(["alice".to_owned()].into())),
            projects,
            store.clone(),
            Arc::new(store.clone()),
        );
        // A sandbox kept by a server before, its record not read.
        let sandbox = store.path(&demo).join("sandbox");
        std::fs::create_dir_all(&sandbox).unwrap();
        std::fs::write(sandbox.join("lost.ext4"), b"").unwrap();
        std::fs::write(sandbox.join("lost.json"), b"not a record").unwrap();
        let (_, home) = access
            .home(&headers(Some("alice"), Some("demo"), None))
            .await
            .unwrap();
        assert_eq!(home.sandboxes().counts().unknown, 1);
        assert!(!access.close(&demo));

        access.forget(&demo);
        assert!(access.homes().is_empty());
        assert!(access.home_of(&demo).await.unwrap().is_none());
        let lock = std::fs::File::open(sandbox.join("lost.ext4.lock")).unwrap();
        assert!(lock.try_lock().is_ok(), "its disk held still");
        assert!(sandbox.join("lost.ext4").exists());
        assert!(sandbox.join("lost.json").exists());
        let pull = http::Request::post("/images/jobs/pull/")
            .header(header::CONTENT_TYPE, "application/json")
            .body(axum::body::Body::from(r#"{"reference": "alpine:3.20"}"#))
            .unwrap();
        let in_home = crate::endpoints::router().layer(axum::Extension(home));
        let Ok(refused) = tower::ServiceExt::oneshot(in_home, pull).await;
        assert_eq!(refused.status(), StatusCode::FORBIDDEN);
    }

    /// Who makes a sandbox of a preset named by its id: a member of its project with their token,
    /// or one its access lets in -- anyone with no credentials where it is public, its subject
    /// their address, and a token of its issuer's for its audience carrying its claims, its
    /// subject the token's -- from the server's origins or the preset's. Refused `401` with a
    /// token no one's, `403` with someone's the preset does not take or a grant, and `404` for
    /// no such preset. One let in names no sandbox, and is held to the preset's limits.
    #[tokio::test]
    async fn a_preset_s_sandbox_is_made_by_its_members_and_whom_its_access_lets_in() {
        use std::time::SystemTime;

        use jsonwebtoken::{Algorithm, EncodingKey, Header};
        use virtx_core::{
            PresetAccess, PresetLimits, PresetPutArgs, PresetSandboxCreateArgs, SandboxCreateArgs,
            SandboxResizeArgs,
        };
        use virtx_uvm::PresetStore;

        use crate::endpoints::Maker;
        use crate::grant::{Grant, Grants};
        use crate::oidc::tests::{KEY, X, Y};

        let keys = json!({"keys": [{
            "kty": "EC", "crv": "P-256", "kid": "k1", "alg": "ES256", "use": "sig", "x": X, "y": Y,
        }]});
        let served = axum::Router::new().route(
            "/keys",
            axum::routing::get(move || async move { keys.to_string() }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let jwks = format!("http://{}/keys", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, served).await });

        let dir = tempfile::tempdir().unwrap();
        let projects = Arc::new(TcpMembership::new(dir.path()));
        let demo = projects
            .create(&Principal::new("alice"), "demo")
            .await
            .unwrap()
            .id;
        let store = ProjectStore::new(dir.path());
        let access = Access::new(
            vec!["https://allowed.example".to_owned()],
            Arc::new(TokenIsUser(["alice".to_owned(), "bob".to_owned()].into())),
            projects,
            store.clone(),
            Arc::new(store.clone()),
        );
        let home = access.home_of(&demo).await.unwrap().unwrap();
        let mut claims = serde_json::Map::new();
        claims.insert("repository".into(), json!("owner/app"));
        let args = PresetPutArgs::new(SandboxCreateArgs::new("app:1"))
            .access(PresetAccess::Public)
            .access(PresetAccess::Jwt {
                issuer: "https://idp.example".into(),
                audience: "app".into(),
                jwks: Some(jwks),
                claims,
            })
            .origin("https://app.example")
            .limits(PresetLimits {
                sandboxes: Some(2),
                per_subject: Some(1),
            });
        let presets = PresetStore::new(&store.path(&demo)).unwrap();
        let (preset, _) = presets
            .put("app", args, &[], || format!("{demo}.p1"))
            .unwrap();

        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap();
        let signed = |audience: &str, repository: &str| {
            let mut header = Header::new(Algorithm::ES256);
            header.kid = Some("k1".into());
            let claims = json!({
                "iss": "https://idp.example", "aud": audience, "exp": now.as_secs() + 600,
                "sub": "user-1", "repository": repository,
            });
            let key = EncodingKey::from_ec_pem(KEY.as_bytes()).unwrap();
            jsonwebtoken::encode(&header, &claims, &key).unwrap()
        };
        let from = "203.0.113.7".parse().ok();
        let maker = async |token: Option<&str>, project, origin, named: &str| {
            let headers = headers(token, project, origin);
            let made = access.maker(&headers, named, from).await;
            made.map(|(_, _, maker)| maker)
                .map_err(|refused| refused.status())
        };
        let id = preset.id.as_str();
        let let_in = |subject: &str| Ok(Maker::LetIn(subject.into()));

        assert_eq!(maker(None, None, None, id).await, let_in("203.0.113.7"));
        assert_eq!(
            maker(Some("alice"), None, None, id).await,
            Ok(Maker::Member("alice".into()))
        );
        assert_eq!(
            maker(Some("alice"), Some("demo"), None, "app").await,
            Ok(Maker::Member("alice".into()))
        );
        let good = signed("app", "owner/app");
        assert_eq!(maker(Some(&good), None, None, id).await, let_in("user-1"));
        let origin = Some("https://app.example");
        assert_eq!(maker(Some(&good), None, origin, id).await, let_in("user-1"));
        let origin = Some("https://allowed.example");
        assert_eq!(maker(None, None, origin, id).await, let_in("203.0.113.7"));

        let refused = async |token, origin, named| maker(token, None, origin, named).await.err();
        let forbidden = Some(StatusCode::FORBIDDEN);
        assert_eq!(
            refused(Some("bob"), None, id).await,
            forbidden,
            "someone's, no member"
        );
        let other = signed("app", "owner/other");
        assert_eq!(
            refused(Some(&other), None, id).await,
            forbidden,
            "not its claims"
        );
        let grant = format!("vxg.{demo}.secret");
        assert_eq!(refused(Some(&grant), None, id).await, forbidden);
        assert_eq!(
            refused(None, Some("https://evil.example"), id).await,
            forbidden
        );
        let unauthorized = Some(StatusCode::UNAUTHORIZED);
        assert_eq!(refused(Some("mallory"), None, id).await, unauthorized);
        let elsewhere = signed("another", "owner/app");
        assert_eq!(
            refused(Some(&elsewhere), None, id).await,
            unauthorized,
            "not its audience"
        );
        let none = format!("{demo}.none");
        assert_eq!(
            refused(None, None, &none).await,
            Some(StatusCode::NOT_FOUND)
        );
        assert_eq!(
            refused(None, None, "app").await,
            unauthorized,
            "by name, members alone"
        );

        // One let in names no sandbox, and makes none past the preset's limits, with those of
        // its sandboxes alive given out: a member's counted toward none.
        let make = async |subject: &str, name: Option<&str>| {
            let args = PresetSandboxCreateArgs {
                name: name.map(str::to_owned),
            };
            let made = home
                .make_of(&preset, Maker::LetIn(subject.into()), args)
                .await;
            made.err().map(|refused| refused.status())
        };
        assert_eq!(make("a", Some("dev")).await, Some(StatusCode::BAD_REQUEST));
        let grants = Grants::new(&store.path(&demo)).unwrap();
        let given = |subject: &str, member| Grant {
            sandbox: format!("of-{subject}"),
            preset: preset.id.clone(),
            subject: subject.into(),
            member,
            max: SandboxResizeArgs::default(),
            origins: Vec::new(),
        };
        grants.give(&demo, &given("alice", true)).unwrap();
        grants.give(&demo, &given("a", false)).unwrap();
        let quota = Some(StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(make("a", None).await, quota, "its own");
        grants.give(&demo, &given("c", false)).unwrap();
        assert_eq!(make("b", None).await, quota, "all of theirs");
        task.abort();
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
