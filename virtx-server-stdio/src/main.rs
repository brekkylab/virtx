//! `virtx-stdio` -- a virtx server over stdio: the endpoints `virtx-core`'s `PROTOCOL.md` names,
//! with the images as an OCI registry at `/images/oci/v2/` (see [`virtx_uvm::RegistryServer`]),
//! answered on stdin and stdout to the client that spawned it, until it closes stdin. Everything
//! this host keeps is that one client's, who owns every project here: its own, `default`, and
//! each one made, each a directory of its own in the cache (see [`virtx_uvm::ProjectStore`]).
//!
//! `virtx-stdio vm …` is this binary spawned by itself to be a machine; see
//! [`virtx_uvm::Machine`].

mod endpoints;

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::Context as _;
use axum::Extension;
use axum::extract::Request;
use axum::middleware::Next;
use axum::response::IntoResponse as _;
use axum::routing::any;
use futures_util::future::BoxFuture;
use http::HeaderMap;
use tower::ServiceExt as _;
use virtx_core::{DEFAULT_PROJECT, PROJECT_HEADER, Role, serve_stdio};
use virtx_uvm::{
    ImageStore, Membership, Owner, Project, ProjectError, ProjectId, ProjectStore, RegistryServer,
};

use crate::endpoints::{Home, Refused, forbidden, invalid};

/// What this binary is spawned with to be a machine.
const MACHINE_SUBCOMMAND: &str = "vm";

/// Where a home's images are served as a registry.
const REGISTRY_PATH: &str = "/images/oci";

fn main() -> anyhow::Result<()> {
    // Before any runtime: this process is about to become a machine and never come back.
    if std::env::args_os()
        .nth(1)
        .is_some_and(|arg| arg == MACHINE_SUBCOMMAND)
    {
        // Why it could not be is the host's already, told on the control channel; stderr is
        // the guest command's, and nothing of this process's goes there.
        let Err(_) = virtx_uvm::enter_machine();
        std::process::exit(1);
    }
    serve()
}

#[tokio::main]
async fn serve() -> anyhow::Result<()> {
    let root = virtx_uvm::cache_root();
    let (app, host) = app(&root).await?;
    let handle = move |request: http::Request<_>| {
        let app = app.clone();
        async move {
            let Ok(response) = app.oneshot(request).await;
            response
        }
    };
    let served = serve_stdio(handle, tokio::io::stdin(), tokio::io::stdout()).await;
    // The client is gone, and every sandbox was its.
    tokio::task::spawn_blocking(move || host.close_all()).await?;
    Ok(served?)
}

/// Everything served over `root`: the endpoints and the images' registry, each answered from the
/// home of the project its request names, and the projects themselves.
async fn app(root: &Path) -> anyhow::Result<(axum::Router, Arc<Host>)> {
    let host = Arc::new(Host::open(root).await?);
    let reading = Arc::clone(&host);
    let images = move |request: Request| {
        let host = Arc::clone(&reading);
        async move {
            match host.home(request.headers()).await {
                Ok(home) => home
                    .registry()
                    .handle(request)
                    .await
                    .map(axum::body::Body::new)
                    .into_response(),
                Err(refused) => refused.into_response(),
            }
        }
    };
    let reaching = Arc::clone(&host);
    let app = endpoints::router()
        .layer(axum::middleware::from_fn(
            move |mut request: Request, next: Next| {
                let host = Arc::clone(&reaching);
                async move {
                    match host.home(request.headers()).await {
                        Ok(home) => {
                            request.extensions_mut().insert(home);
                            next.run(request).await
                        }
                        Err(refused) => refused.into_response(),
                    }
                }
            },
        ))
        .merge(endpoints::projects().layer(Extension(Arc::clone(&host))))
        .route("/images/oci/v2", any(images.clone()))
        .route("/images/oci/v2/", any(images.clone()))
        .route("/images/oci/v2/{*rest}", any(images));
    Ok((app, host))
}

/// The one user here, the client, whose every project is.
const CLIENT: &str = "client";

/// The projects this host keeps, each the client's, the home of each made as it is first
/// reached.
pub struct Host {
    membership: StdioMembership,
    store: ProjectStore,
    /// Each project's home, by id.
    /// `None` for a project being removed, which no request reaches until it is gone.
    homes: Mutex<HashMap<ProjectId, Option<Arc<Home>>>>,
}

impl Host {
    /// The projects under `root`, what was kept at `root` before moved into the client's own
    /// (see [`adopt_unprojected`]), and the home of each made now, so that its sandboxes left
    /// from before are swept as the server starts.
    async fn open(root: &Path) -> anyhow::Result<Host> {
        let host = Host {
            membership: StdioMembership::new(root),
            store: ProjectStore::new(root),
            homes: Mutex::default(),
        };
        let own = host
            .project(None)
            .await?
            .context("the client's own project")?;
        adopt_unprojected(root, &host.store.path(&own.id))?;
        for id in host.store.list()? {
            host.home_of(&id)?;
        }
        Ok(host)
    }

    pub fn membership(&self) -> &StdioMembership {
        &self.membership
    }

    pub fn store(&self) -> &ProjectStore {
        &self.store
    }

    /// The client's project `named` names, if any: its own, [`DEFAULT_PROJECT`], where it is
    /// `None`; the project of that id, where it is one; and otherwise the project of that name.
    pub async fn project(&self, named: Option<&str>) -> Result<Option<Project>, ProjectError> {
        let named = named.unwrap_or(DEFAULT_PROJECT);
        let id = match named.parse::<ProjectId>() {
            Ok(id) => id,
            Err(_) => match self.membership.resolve(&client(), named).await? {
                Some(id) => id,
                None => return Ok(None),
            },
        };
        let project = self.membership.project(&id).await?;
        Ok(project.filter(|project| project.members.contains_key(CLIENT)))
    }

    /// The home of the project `headers` name, or of the client's own where they name none:
    /// `403` for a project not here.
    async fn home(&self, headers: &HeaderMap) -> Result<Arc<Home>, Refused> {
        let named = match headers.get(PROJECT_HEADER) {
            None => None,
            Some(named) => Some(
                named
                    .to_str()
                    .map_err(|_| invalid("the virtx-project header is not a project's name"))?,
            ),
        };
        let project = self
            .project(named)
            .await
            .map_err(endpoints::internal)?
            .ok_or_else(|| forbidden(format!("no project here is {named:?}")))?;
        let home = self.home_of(&project.id).map_err(|e| {
            endpoints::internal(format!("making project {}'s home: {e:#}", project.name))
        })?;
        home.ok_or_else(|| forbidden(format!("project {} is being removed", project.name)))
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
    /// request reaches it from now until [`release`](Host::release). `false`, and nothing
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

    /// Close every sandbox of every home, as removing each does: blocks until each machine has
    /// stopped and synced what it wrote.
    fn close_all(&self) {
        let homes: Vec<_> = self
            .homes
            .lock()
            .unwrap()
            .values()
            .flatten()
            .cloned()
            .collect();
        for home in homes {
            home.close_all();
        }
    }
}

/// The client, as the owner of its projects.
fn client() -> Owner {
    Owner::User(CLIENT.to_owned())
}

/// The client's projects, each kept in its directory as `project.json`, the [`Project`], the
/// client its one member: a project is there while that file is. Each change is made holding
/// `project/.lock`, so that a `virtx-stdio` for each client over one root changes them one at a
/// time.
pub struct StdioMembership {
    store: ProjectStore,
}

impl StdioMembership {
    fn new(root: &Path) -> StdioMembership {
        StdioMembership {
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

    /// Make project `name` of `owner`'s, `creator` its owner, holding the lock: written beside,
    /// and renamed over.
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
        let dir = self.store.path(&project.id);
        fs::create_dir_all(&dir)
            .map_err(|e| ProjectError::io(format!("making {}", dir.display()), e))?;
        let (path, part) = (dir.join("project.json"), dir.join("project.json.part"));
        let bytes = serde_json::to_vec_pretty(&project).expect("JSON of strings");
        fs::write(&part, bytes)
            .map_err(|e| ProjectError::io(format!("writing {}", part.display()), e))?;
        fs::rename(&part, &path)
            .map_err(|e| ProjectError::io(format!("writing {}", path.display()), e))?;
        Ok(project)
    }
}

impl Membership for StdioMembership {
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
                // Made by a `virtx-stdio` beside this one.
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
            reached.sort_by(|a, b| a.name.cmp(&b.name));
            Ok(reached)
        })
    }

    /// Refused: the client is the one member of each project here.
    fn set_member<'a>(
        &'a self,
        id: &'a ProjectId,
        _user: &'a str,
        _role: Option<Role>,
    ) -> BoxFuture<'a, Result<(), ProjectError>> {
        Box::pin(async move { Err(ProjectError::NoOwner(id.to_string())) })
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

/// Move what this host kept before it kept projects, at `root` itself, into `default`, the
/// directory of the client's own project: once, while it has none of it.
fn adopt_unprojected(root: &Path, default: &Path) -> anyhow::Result<()> {
    const KEPT: [&str; 4] = ["images", "snapshot", "sandbox", "volume"];
    if KEPT.iter().any(|kept| default.join(kept).exists()) {
        return Ok(());
    }
    for kept in KEPT {
        let from = root.join(kept);
        if !from.exists() {
            continue;
        }
        fs::create_dir_all(default).with_context(|| format!("making {}", default.display()))?;
        match fs::rename(&from, default.join(kept)) {
            Ok(()) => {}
            // Another `virtx-stdio`, starting beside this one, moved it first.
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(e).with_context(|| {
                    format!("moving {} into the client's own project", from.display())
                });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    //! The app, served in the test's own process as over stdio, and called as a client calls it:
    //! the endpoints over a `VIRTX_HOME` of the test's own, and the files of its volumes and
    //! snapshots among them.

    use std::{
        path::{Path, PathBuf},
        sync::Arc,
    };

    use virtx_core::{
        Client, DirEntry, Error, FileRoot, ImageName, MountSource, MountType, Recipe,
        SandboxCreateArgs, SandboxExecArgs, SandboxVolume,
    };
    use virtx_uvm::{Entry, FsTree, ImageStore};

    use super::*;

    /// The app over a root of its own, served as over stdio, and a client of it; the root is
    /// removed when this drops.
    struct Server {
        client: Client,
        root: PathBuf,
        /// The root of the client's own project, which it reaches naming none.
        home: PathBuf,
        /// The app the client is served, for a request no client sends.
        app: axum::Router,
    }

    impl Server {
        async fn spawn(name: &str) -> Server {
            Server::spawn_after(name, |_, _| {}).await
        }

        /// `spawn`, with `after` run once the app has answered each request, given the client's
        /// own project's root and the request's path: for what another client does between a client's
        /// requests.
        async fn spawn_after(
            name: &str,
            after: impl Fn(&Path, &str) + Send + Sync + 'static,
        ) -> Server {
            let root =
                std::env::temp_dir().join(format!("virtx-stdio-app-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            let (app, host) = app(&root).await.unwrap();
            let own = host.project(None).await.unwrap().unwrap();
            let home = host.store().path(&own.id);
            let served_app = app.clone();
            let after = Arc::new(after);
            let served = home.clone();
            let handle = move |request: http::Request<_>| {
                let app = app.clone();
                let after = Arc::clone(&after);
                let home = served.clone();
                async move {
                    let path = request.uri().path().to_owned();
                    let Ok(response) = app.oneshot(request).await;
                    after(&home, &path);
                    response
                }
            };
            let (pipe, server) = tokio::io::duplex(64 << 10);
            let (input, output) = tokio::io::split(server);
            tokio::spawn(serve_stdio(handle, input, output));
            Server {
                client: Client::over_stream(pipe),
                root,
                home,
                app: served_app,
            }
        }

        /// File an empty image under `tag` in the store the server reads, and answer its digest.
        fn file_image(&self, tag: &str) -> String {
            let store = ImageStore::new(self.home.join("images")).unwrap();
            let layer = store.insert_layer(&FsTree::new()).unwrap();
            let config = serde_json::from_str(
                r#"{"architecture": "arm64", "os": "linux", "rootfs": {"type": "layers", "diff_ids": []}}"#,
            )
            .unwrap();
            let image = store.insert_image(&config, &[layer]).unwrap();
            store.insert_tag(&tag.parse().unwrap(), &image).unwrap();
            image.to_string()
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn status<T: std::fmt::Debug>(answered: Result<T, Error>) -> u16 {
        // Either kind: a Files refusal is plain HTTP, not the protocol's error body.
        match answered {
            Err(ref e) if e.status().is_some() => e.status().expect("checked"),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    fn names(entries: Vec<DirEntry>) -> Vec<String> {
        entries.into_iter().map(|entry| entry.name).collect()
    }

    /// A snapshot held by this process, as a machine over it would hold it.
    fn hold(server: &Server, snapshot: &str) -> std::fs::File {
        let lock =
            std::fs::File::create(server.home.join(format!("snapshot/{snapshot}.ext4.lock")))
                .unwrap();
        lock.try_lock().unwrap();
        lock
    }

    #[tokio::test]
    async fn what_cannot_be_built_is_refused_before_it_starts() {
        let server = Server::spawn("build-refused").await;
        let built = server
            .client
            .build_image(Recipe::new("alpine:3.20"), "not a reference")
            .await;
        assert_eq!(status(built), 400);
        let built = server
            .client
            .build_image(Recipe::new("Not A Base!"), "built:latest")
            .await;
        assert_eq!(status(built), 400);
        assert_eq!(server.client.list_image_job().await.unwrap(), []);
    }

    #[tokio::test]
    async fn creates_lists_and_removes_a_snapshot() {
        let server = Server::spawn("snapshot").await;
        let created = server
            .client
            .create_snapshot(Some("work"), Some(1))
            .await
            .unwrap();
        assert_eq!(created.name, "work");
        assert!(!created.in_use);

        let listed = server.client.list_snapshot().await.unwrap();
        assert_eq!(listed, std::slice::from_ref(&created));
        assert_eq!(server.client.get_snapshot("work").await.unwrap(), created);
        assert!(server.home.join("snapshot/work.ext4").is_file());

        server.client.remove_snapshot("work").await.unwrap();
        assert_eq!(server.client.list_snapshot().await.unwrap(), []);
        assert_eq!(status(server.client.remove_snapshot("work").await), 404);
        assert_eq!(status(server.client.get_snapshot("work").await), 404);
    }

    #[tokio::test]
    async fn what_cannot_be_a_snapshot_is_refused() {
        let server = Server::spawn("snapshot-refused").await;
        let create = async |name: Option<&str>, disk_gib| {
            server.client.create_snapshot(name, disk_gib).await
        };
        create(Some("taken"), Some(1)).await.unwrap();

        assert_eq!(status(create(Some("taken"), Some(1)).await), 409);
        assert_eq!(status(create(Some("zero"), Some(0)).await), 400);
        for name in ["../out", ".hidden", "-flag", "spa ce"] {
            assert_eq!(status(create(Some(name), Some(1)).await), 400, "{name:?}");
        }

        // One with no name is given one.
        let picked = create(None, None).await.unwrap().name;
        let listed = server.client.list_snapshot().await.unwrap();
        assert!(listed.iter().any(|item| item.name == picked), "{listed:?}");
    }

    #[tokio::test]
    async fn a_snapshot_in_use_is_listed_so_and_not_removed() {
        let server = Server::spawn("snapshot-held").await;
        server
            .client
            .create_snapshot(Some("held"), Some(1))
            .await
            .unwrap();

        // Locked by this process, as a VM over it would: the lock is the file beside the snapshot.
        let lock = std::fs::File::create(server.home.join("snapshot/held.ext4.lock")).unwrap();
        lock.try_lock().unwrap();
        let listed = server.client.list_snapshot().await.unwrap();
        assert!(listed[0].in_use);
        assert_eq!(status(server.client.remove_snapshot("held").await), 409);

        drop(lock);
        server.client.remove_snapshot("held").await.unwrap();
        assert!(!server.home.join("snapshot/held.ext4.lock").exists());
    }

    #[tokio::test]
    async fn creates_lists_and_removes_a_volume() {
        let server = Server::spawn("volume").await;
        let create = async |name: Option<&str>| server.client.create_volume(name).await;
        assert_eq!(create(Some("data")).await.unwrap().name, "data");
        assert_eq!(status(create(Some("data")).await), 409);
        assert_eq!(status(create(Some("a b")).await), 400);
        let picked = create(None).await.unwrap().name;

        let mut listed: Vec<_> = server
            .client
            .list_volume()
            .await
            .unwrap()
            .into_iter()
            .map(|item| item.name)
            .collect();
        listed.sort();
        let mut made = vec!["data".to_string(), picked];
        made.sort();
        assert_eq!(listed, made);
        assert_eq!(server.client.get_volume("data").await.unwrap().name, "data");

        // Held by this process, as a VM that mounts it would.
        let lock_path = server.home.join("volume/data/lock");
        let lock = std::fs::File::open(&lock_path)
            .or_else(|_| std::fs::File::create(&lock_path))
            .unwrap();
        lock.lock_shared().unwrap();
        assert_eq!(status(server.client.remove_volume("data").await), 409);
        drop(lock);
        server.client.remove_volume("data").await.unwrap();
        assert_eq!(status(server.client.remove_volume("data").await), 404);
    }

    #[tokio::test]
    async fn a_mount_is_kept_at_a_path_until_it_is_removed() {
        let server = Server::spawn("mount").await;
        server.client.create_volume(Some("v")).await.unwrap();
        let mount = async |path: &str| {
            let source = MountSource::Notion {
                api_key: "secret".into(),
            };
            server.client.create_volume_mount("v", path, source).await
        };
        let mounts = async || {
            let listed = server.client.list_volume_mount("v").await.unwrap();
            listed
                .into_iter()
                .map(|m| (m.path, m.mount_type))
                .collect::<Vec<_>>()
        };

        let made = mount("notes").await.unwrap();
        assert_eq!(
            (made.path, made.mount_type),
            ("notes".into(), MountType::Notion)
        );
        assert_eq!(mounts().await, [("notes".to_owned(), MountType::Notion)]);
        assert_eq!(status(mount("notes").await), 409);
        assert_eq!(status(mount("no/parent").await), 404);

        server
            .client
            .remove_volume_mount("v", "notes")
            .await
            .unwrap();
        assert_eq!(mounts().await, []);
        assert_eq!(
            status(server.client.remove_volume_mount("v", "notes").await),
            404
        );
        assert_eq!(status(server.client.list_volume_mount("nobody").await), 404);
    }

    /// Refused before any machine is booted.
    #[tokio::test]
    async fn a_sandbox_that_cannot_be_is_refused_before_it_boots() {
        let server = Server::spawn("sandbox-refused").await;
        let digest = server.file_image("here:latest");
        let refused =
            async |args: SandboxCreateArgs| status(server.client.create_sandbox(&args).await);
        let here = || SandboxCreateArgs::new("here:latest");

        for ports in [vec![0], vec![8080, 8080]] {
            let mut args = here();
            args.ports = ports;
            assert_eq!(refused(args).await, 400);
        }
        let mut args = here();
        args.vcpus = Some(0);
        assert_eq!(refused(args).await, 400);
        for mib in [0, 16, 32] {
            let mut args = here();
            args.memory_mib = Some(mib);
            assert_eq!(refused(args).await, 400, "memory_mib {mib}");
        }
        let mut args = here();
        args.gpu_memory_mib = Some(1024);
        assert_eq!(refused(args).await, 400);

        let missing = SandboxCreateArgs::new("nothing:here");
        let missing = server.client.create_sandbox(&missing).await.unwrap_err();
        assert_eq!(missing.status(), Some(404), "{missing}");
        assert!(
            missing
                .to_string()
                .ends_with("no image here is tagged nothing:here: pull it first"),
            "{missing}"
        );
        let absent = format!("sha256:{}", "0".repeat(64));
        assert_ne!(absent, digest);
        assert_eq!(
            refused(SandboxCreateArgs::new(ImageName::parse(&absent))).await,
            404
        );
        let mut args = here();
        args.snapshot = Some("missing".into());
        assert_eq!(refused(args).await, 404);
        let mut args = here();
        args.volumes = vec![SandboxVolume {
            volume: "missing".into(),
            path: "/data".into(),
            readonly: false,
        }];
        assert_eq!(refused(args).await, 404);
        server.client.create_volume(Some("data")).await.unwrap();
        let at = |path: &str| SandboxVolume {
            volume: "data".into(),
            path: path.into(),
            readonly: false,
        };
        // Each as the guest reads it: `.` and repeated `/` resolve, so the root and one path
        // spelled another way are what they are spelled as.
        for volumes in [
            vec![at("/data"), at("/data/")],
            vec![at("/data"), at("//data")],
            vec![at("data")],
            vec![at("/")],
            vec![at("/.")],
            vec![at("/./")],
            vec![at("/..")],
            vec![at("/data"), at("/data/.")],
            vec![at("/data/../etc")],
        ] {
            let spelled: Vec<_> = volumes.iter().map(|v| v.path.clone()).collect();
            let mut args = here();
            args.volumes = volumes;
            assert_eq!(refused(args).await, 400, "{spelled:?}");
        }
        for name in ["nope", "skills", "../virtx-stdio"] {
            assert_eq!(refused(here().abin(name)).await, 400, "{name}");
        }
        match server
            .client
            .create_sandbox(&here().abin("index").abin("index"))
            .await
        {
            Err(Error::Status(400, body)) => assert!(body.contains("given twice"), "{body}"),
            other => panic!("expected a refusal, got {other:?}"),
        }

        assert_eq!(server.client.list_sandbox().await.unwrap(), []);
        let exec = async |cmd: &[&str]| {
            let args = SandboxExecArgs::new(cmd.iter().copied());
            server.client.exec_sandbox("nobody", &args).await
        };
        assert_eq!(status(exec(&["true"]).await), 404);
        assert_eq!(status(exec(&[]).await), 400);
        assert_eq!(status(server.client.remove_sandbox("nobody").await), 404);
    }

    #[tokio::test]
    async fn the_files_of_a_volume_are_written_listed_moved_and_read_back() {
        let server = Server::spawn("volume-files").await;
        server.client.create_volume(Some("data")).await.unwrap();
        let client = &server.client;
        let volume = FileRoot::Volume("data".into());

        client.create_dir(&volume, "dir").await.unwrap();
        client
            .write_file(&volume, "dir/a.txt", "hello world", None)
            .await
            .unwrap();
        assert!(server.home.join("volume/data/files/dir/a.txt").is_file());

        let metadata = client.stat_file(&volume, "dir/a.txt").await.unwrap();
        assert!(!metadata.is_dir && metadata.size == 11, "{metadata:?}");
        assert!(metadata.modified.is_some());
        let entries = client.read_dir(&volume, "dir").await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "a.txt");
        assert_eq!(entries[0].metadata, metadata);
        assert_eq!(names(client.read_dir(&volume, "").await.unwrap()), ["dir"]);

        let read = client
            .read_file(&volume, "dir/a.txt", 0, None)
            .await
            .unwrap();
        assert_eq!(&read[..], b"hello world");
        let read = client
            .read_file(&volume, "dir/a.txt", 6, Some(3))
            .await
            .unwrap();
        assert_eq!(&read[..], b"wor");

        client
            .rename_file(&volume, "dir/a.txt", "b.txt", false)
            .await
            .unwrap();
        client
            .copy_file(&volume, "b.txt", "c.txt", false)
            .await
            .unwrap();
        assert_eq!(
            status(client.copy_file(&volume, "b.txt", "c.txt", false).await),
            412
        );
        client
            .copy_file(&volume, "b.txt", "c.txt", true)
            .await
            .unwrap();
        client
            .copy_file(&volume, "dir", "copied", false)
            .await
            .unwrap();
        client.remove_file(&volume, "dir", false).await.unwrap();
        assert_eq!(status(client.stat_file(&volume, "dir").await), 404);
        assert!(client.stat_file(&volume, "copied").await.unwrap().is_dir);
        let read = client.read_file(&volume, "c.txt", 0, None).await.unwrap();
        assert_eq!(&read[..], b"hello world");

        // What RFC 4918 refuses, with its status.
        assert_eq!(
            status(client.write_file(&volume, "no/file", "x", None).await),
            409
        );
        assert_eq!(status(client.create_dir(&volume, "no/dir").await), 409);
        assert_eq!(status(client.create_dir(&volume, "copied").await), 405);
        assert_eq!(
            status(client.read_file(&volume, "missing", 0, None).await),
            404
        );
        assert_eq!(
            status(client.remove_file(&volume, "missing", false).await),
            404
        );
        let nobody = FileRoot::Volume("nobody".into());
        assert_eq!(status(client.stat_file(&nobody, "").await), 404);

        // `mkdir -p` of a path with a dot segment creates the parents of the path it resolves
        // to, and no others.
        client
            .create_dir_all(&volume, "discard/../parent/child")
            .await
            .unwrap();
        assert!(
            client
                .stat_file(&volume, "parent/child")
                .await
                .unwrap()
                .is_dir
        );
        let mut top = names(client.read_dir(&volume, "").await.unwrap());
        top.sort();
        assert_eq!(top, ["b.txt", "c.txt", "copied", "parent"]);

        // The root is removed through its resource, and a request that would take it, or what is in
        // it, is refused before anything goes.
        client
            .write_file(&volume, "kept.txt", "kept", None)
            .await
            .unwrap();
        assert_eq!(status(client.remove_file(&volume, "", false).await), 403);
        assert_eq!(
            status(client.copy_file(&volume, "copied", "", true).await),
            403
        );
        assert_eq!(client.read_text(&volume, "kept.txt").await.unwrap(), "kept");
        // A client resolves a dot segment before it sends a path, as the SDK does, so the root
        // has one spelling; one that arrives with a dot segment, or out of the volume, is refused
        // before anything goes.
        assert_eq!(status(client.remove_file(&volume, ".", false).await), 403);
        assert_eq!(
            status(client.remove_file(&volume, "copied/..", false).await),
            403
        );
        let raw = async |method: &str, path: &str, to: Option<&str>| {
            let mut request = http::Request::builder().method(method).uri(path);
            if let Some(to) = to {
                request = request.header("Destination", to);
            }
            let request = request.body(axum::body::Body::empty()).unwrap();
            let Ok(response) = server.app.clone().oneshot(request).await;
            response.status().as_u16()
        };
        for dotted in [
            "/volumes/data/files/.",
            "/volumes/data/files/%2e",
            "/volumes/data/files/copied/..",
        ] {
            assert_eq!(raw("DELETE", dotted, None).await, 400, "DELETE {dotted}");
            let copied = "/volumes/data/files/copied";
            assert_eq!(
                raw("MOVE", copied, Some(dotted)).await,
                400,
                "MOVE onto {dotted}"
            );
        }
        assert_eq!(
            raw("PROPFIND", "/volumes/data/files/../../nobody/files", None).await,
            400
        );
        assert_eq!(
            raw(
                "COPY",
                "/volumes/data/files/kept.txt",
                Some("/volumes/nobody/files/x")
            )
            .await,
            400
        );
        assert_eq!(client.read_text(&volume, "kept.txt").await.unwrap(), "kept");
        assert!(client.stat_file(&volume, "copied").await.unwrap().is_dir);

        // A link in the volume, which a machine may have made, points at nothing here.
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/", server.home.join("volume/data/files/out")).unwrap();
            assert_eq!(status(client.stat_file(&volume, "out").await), 404);
            assert_eq!(status(client.read_dir(&volume, "out").await), 404);
            assert!(
                !names(client.read_dir(&volume, "").await.unwrap()).contains(&"out".to_owned())
            );
        }
    }

    #[tokio::test]
    async fn the_files_of_a_snapshot_are_kept_in_its_image() {
        let server = Server::spawn("snapshot-files").await;
        server
            .client
            .create_snapshot(Some("work"), Some(1))
            .await
            .unwrap();
        let client = &server.client;
        let snapshot = FileRoot::Snapshot("work".into());

        // What the format made, and nothing else.
        assert_eq!(
            names(client.read_dir(&snapshot, "").await.unwrap()),
            [] as [&str; 0]
        );

        client.create_dir(&snapshot, "etc").await.unwrap();
        client
            .write_file(&snapshot, "etc/hostname", "guest\n", None)
            .await
            .unwrap();
        assert_eq!(
            status(client.write_file(&snapshot, "no/file", "x", None).await),
            409
        );
        assert_eq!(status(client.create_dir(&snapshot, "etc").await), 405);
        let read = client
            .read_file(&snapshot, "etc/hostname", 0, None)
            .await
            .unwrap();
        assert_eq!(&read[..], b"guest\n");
        let read = client
            .read_file(&snapshot, "etc/hostname", 1, Some(2))
            .await
            .unwrap();
        assert_eq!(&read[..], b"ue");
        let metadata = client.stat_file(&snapshot, "etc/hostname").await.unwrap();
        assert_eq!((metadata.is_dir, metadata.size), (false, 6));
        assert!(metadata.modified.is_some());
        assert_eq!(
            names(client.read_dir(&snapshot, "etc").await.unwrap()),
            ["hostname"]
        );

        client
            .rename_file(&snapshot, "etc/hostname", "hostname", false)
            .await
            .unwrap();
        client
            .copy_file(&snapshot, "hostname", "etc/copy", false)
            .await
            .unwrap();
        assert_eq!(
            status(
                client
                    .copy_file(&snapshot, "hostname", "etc/copy", false)
                    .await
            ),
            412
        );
        client
            .remove_file(&snapshot, "hostname", false)
            .await
            .unwrap();
        assert_eq!(status(client.stat_file(&snapshot, "hostname").await), 404);

        // In the image, as the next machine over it finds it.
        let image = std::fs::File::open(server.home.join("snapshot/work.ext4")).unwrap();
        let tree = FsTree::from_ext4(image).unwrap();
        let paths: Vec<_> = tree
            .walk()
            .into_iter()
            .map(|(path, ..)| String::from_utf8(path).unwrap())
            .collect();
        assert_eq!(
            paths,
            ["", "lost+found", "upper", "upper/etc", "upper/etc/copy"]
        );
        assert!(matches!(
            tree.entry(b"upper/etc/copy").unwrap(),
            Some(Entry::File { data, .. }) if data == b"guest\n"
        ));

        // At once: each request reads and writes the image whole, one after another.
        let paths = ["etc/0", "etc/1", "etc/2", "etc/3"];
        let writes = paths
            .iter()
            .map(|path| client.write_file(&snapshot, path, "x", None));
        for written in futures_util::future::join_all(writes).await {
            written.unwrap();
        }
        assert_eq!(
            names(client.read_dir(&snapshot, "etc").await.unwrap()),
            ["0", "1", "2", "3", "copy"]
        );

        // Held by a machine: nothing is read or written until it is let go.
        let held = hold(&server, "work");
        assert_eq!(status(client.read_dir(&snapshot, "").await), 423);
        assert_eq!(
            status(client.write_file(&snapshot, "x", "y", None).await),
            423
        );
        drop(held);
        client.remove_file(&snapshot, "etc", false).await.unwrap();
        assert_eq!(
            names(client.read_dir(&snapshot, "").await.unwrap()),
            [] as [&str; 0]
        );
        let nobody = FileRoot::Snapshot("nobody".into());
        assert_eq!(status(client.read_dir(&nobody, "").await), 404);
    }

    /// The images are an OCI registry at `/images/oci/v2/` on the same stdio, beside the other
    /// endpoints.
    #[tokio::test]
    async fn the_images_are_a_registry_beside_the_endpoints() {
        let server = Server::spawn("registry").await;
        let digest = server.file_image("alpine:3.21");
        let listed = server.client.list_image().await.unwrap();
        assert_eq!(listed.len(), 1, "{listed:?}");
        assert_eq!(listed[0].digest, digest);
    }

    /// A prune takes an image no tag names once it is old, and what only it held, and leaves a
    /// tagged one.
    /// A list is three requests: the catalog, each repository's tags, each tag's manifest.
    /// Between the first two, another client removes a repository's only tag.
    #[tokio::test]
    async fn a_repository_removed_mid_list_is_left_out() {
        let server = Server::spawn_after("list-race", |home, path| {
            if path == "/images/oci/v2/_catalog" {
                let store = ImageStore::new(home.join("images")).unwrap();
                store.remove_tag(&"gone:latest".parse().unwrap()).unwrap();
            }
        })
        .await;
        let kept = server.file_image("kept:latest");
        server.file_image("gone:latest");
        let listed = server.client.list_image().await.unwrap();
        assert_eq!(
            listed
                .iter()
                .map(|image| (image.reference.as_str(), image.digest.as_str()))
                .collect::<Vec<_>>(),
            [("docker.io/library/kept:latest", kept.as_str())]
        );
    }

    #[tokio::test]
    async fn a_prune_takes_an_old_untagged_image() {
        let server = Server::spawn("prune").await;
        let tagged = server.file_image("app:v1");
        let store = ImageStore::new(server.home.join("images")).unwrap();
        let mut tree = FsTree::new();
        tree.insert(
            b"loose",
            virtx_uvm::Entry::Directory {
                meta: Default::default(),
                opaque: false,
            },
        )
        .unwrap();
        let layer = store.insert_layer(&tree).unwrap();
        let loose = store
            .insert_image(&Default::default(), std::slice::from_ref(&layer))
            .unwrap();
        // Older than the prune's grace, as everything under the store now is.
        let hours_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 60 * 60);
        let mut dirs = vec![store.root().to_owned()];
        while let Some(dir) = dirs.pop() {
            for entry in std::fs::read_dir(dir).unwrap().flatten() {
                if entry.file_type().unwrap().is_dir() {
                    dirs.push(entry.path());
                } else {
                    let file = std::fs::File::options()
                        .write(true)
                        .open(entry.path())
                        .unwrap();
                    file.set_modified(hours_ago).unwrap();
                }
            }
        }

        let pruned = server.client.prune_image().await.unwrap();
        assert_eq!(pruned.images, vec![loose.to_string()]);
        assert!(pruned.reclaimed > 0);
        assert!(!store.contains_layer(&layer));
        assert!(store.get_image(&tagged.parse().unwrap()).unwrap().is_some());
    }

    /// The client owns every project: its own, `_`, there from the start, and each it makes. A
    /// project's things are its own, and go with it.
    #[tokio::test]
    async fn the_client_makes_reaches_and_removes_its_projects() {
        let server = Server::spawn("projects").await;
        let send = async |method: &str, path: &str, project: Option<&str>, body: &str| {
            let mut request = http::Request::builder().method(method).uri(path);
            if let Some(project) = project {
                request = request.header("virtx-project", project);
            }
            let request = request
                .header("content-type", "application/json")
                .body(axum::body::Body::from(body.to_owned()))
                .unwrap();
            let Ok(response) = server.app.clone().oneshot(request).await;
            let status = response.status().as_u16();
            let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            (status, String::from_utf8(body.to_vec()).unwrap())
        };
        let status = async |method, path, project, body| send(method, path, project, body).await.0;

        let listed = async || -> Vec<(String, String)> {
            let listed = send("GET", "/projects/", None, "").await.1;
            let listed: serde_json::Value = serde_json::from_str(&listed).unwrap();
            let listed = listed.as_array().unwrap().iter();
            listed
                .map(|project| {
                    assert_eq!(project["role"], "owner");
                    assert!(project.get("owner").is_none());
                    let field = |name: &str| project[name].as_str().unwrap().to_owned();
                    (field("name"), field("id"))
                })
                .collect()
        };
        let own = listed().await;
        assert_eq!(own.len(), 1);
        assert_eq!(own[0].0, "default");
        let made = send("POST", "/projects/", None, r#"{"name": "demo"}"#).await;
        assert_eq!(made.0, 201);
        let made: serde_json::Value = serde_json::from_str(&made.1).unwrap();
        let id = made["id"].as_str().unwrap().to_owned();
        assert_eq!(
            (&made["name"], &made["role"]),
            (&"demo".into(), &"owner".into())
        );
        assert_eq!(
            status("POST", "/projects/", None, r#"{"name": "demo"}"#).await,
            409
        );
        // `default` is the client's own, and an id names no other.
        for theirs in [
            r#"{"name": "default"}"#,
            r#"{"name": "Demo"}"#,
            r#"{"name": "0f6a3b8e-1c2d-4e5f-8a9b-0c1d2e3f4a5b"}"#,
        ] {
            assert_eq!(
                status("POST", "/projects/", None, theirs).await,
                400,
                "{theirs}"
            );
        }
        let names: Vec<_> = listed().await.into_iter().map(|(name, _)| name).collect();
        assert_eq!(names, ["default", "demo"]);
        assert_eq!(status("GET", "/projects/demo", None, "").await, 200);
        let by_id = format!("/projects/{id}");
        assert_eq!(status("GET", &by_id, None, "").await, 200);
        assert_eq!(status("GET", "/projects/nowhere", None, "").await, 403);

        // Each project's things are its own, by its name or its id, and a project not here is
        // reached by no request.
        let volume = r#"{"name": "data"}"#;
        assert_eq!(status("POST", "/volumes/", Some("demo"), volume).await, 201);
        assert_ne!(send("GET", "/volumes/", Some(&id), "").await.1, "[]");
        assert_eq!(send("GET", "/volumes/", None, "").await.1, "[]");
        assert_eq!(status("GET", "/volumes/", Some("nowhere"), "").await, 403);
        let dir = server.root.join("project").join(&id);
        assert!(dir.join("volume/data/files").is_dir());

        // No one but the client: a project here has no members to change.
        assert_eq!(
            send("GET", "/projects/demo/members/", None, "").await.1,
            "[]"
        );
        let member = r#"{"role": "member"}"#;
        assert_eq!(
            status("PUT", "/projects/demo/members/alice", None, member).await,
            403
        );

        // A project goes with everything in it; the client's own is made again, empty.
        assert_eq!(status("DELETE", "/projects/demo", None, "").await, 204);
        assert!(!dir.exists());
        assert_eq!(status("GET", "/volumes/", Some("demo"), "").await, 403);
        assert_eq!(status("POST", "/volumes/", None, volume).await, 201);
        assert_eq!(status("DELETE", "/projects/default", None, "").await, 204);
        assert_eq!(send("GET", "/volumes/", None, "").await.1, "[]");
        let again = listed().await;
        assert_eq!(again.len(), 1);
        assert_eq!(again[0].0, "default");
        assert_ne!(again[0].1, own[0].1);
    }

    #[test]
    fn what_was_kept_before_projects_is_the_client_s_own() {
        let root = std::env::temp_dir().join(format!("virtx-stdio-adopt-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("volume/data/files")).unwrap();
        fs::create_dir_all(root.join("images/tag")).unwrap();
        fs::create_dir_all(root.join("virtx-stdio/0.1.0")).unwrap();
        let default = root.join("project/own");

        adopt_unprojected(&root, &default).unwrap();
        assert!(default.join("volume/data/files").is_dir());
        assert!(default.join("images/tag").is_dir());
        assert!(!root.join("volume").exists() && !root.join("images").exists());
        // What is no project's stays where it was.
        assert!(root.join("virtx-stdio/0.1.0").is_dir());

        // Once only: what turns up at the root after is left there.
        fs::create_dir_all(root.join("volume")).unwrap();
        adopt_unprojected(&root, &default).unwrap();
        assert!(root.join("volume").is_dir());
        let _ = fs::remove_dir_all(&root);
    }
}
