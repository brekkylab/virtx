//! `virtx-stdio` -- a virtx server over stdio: the endpoints `virtx-core`'s `PROTOCOL.md` names,
//! with the images as an OCI registry at `/images/oci/v2/` (see [`virtx_uvm::RegistryServer`]),
//! answered on stdin and stdout to the client that spawned it, until it closes stdin. Everything
//! this host keeps is that one client's, in every project: `default`, and each one made, under
//! its own root in the cache's (see [`virtx_uvm::project_root`]). No one is authenticated over
//! stdio, so the client owns every one ([`OpenProjects`]).
//!
//! `virtx-stdio vm …` is this binary spawned by itself to be a machine; see
//! [`virtx_uvm::Machine`].

mod endpoints;

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Context as _;
use axum::extract::Request;
use axum::middleware::Next;
use axum::response::IntoResponse as _;
use axum::routing::any;
use futures_util::future::BoxFuture;
use http::{HeaderMap, StatusCode};
use tower::ServiceExt as _;
use virtx_core::{DEFAULT_PROJECT, PROJECT_HEADER, Role, serve_stdio};
use virtx_uvm::{
    ImageStore, ProjectError, ProjectStore, RegistryServer, project_names, project_root,
};

use crate::endpoints::{Home, Refused, refused};

/// What this binary is spawned with to be a machine.
const MACHINE_SUBCOMMAND: &str = "vm";

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
    adopt_unprojected(&root)?;
    let (app, homes) = app(&root).await?;
    let handle = move |request: http::Request<_>| {
        let app = app.clone();
        async move {
            let Ok(response) = app.oneshot(request).await;
            response
        }
    };
    let served = serve_stdio(handle, tokio::io::stdin(), tokio::io::stdout()).await;
    // The client is gone, and every sandbox was its.
    tokio::task::spawn_blocking(move || homes.close_all()).await?;
    Ok(served?)
}

/// Everything served over `root`: the endpoints and the images' registry, each answered from the
/// home of the project its request names.
async fn app(root: &Path) -> anyhow::Result<(axum::Router, Arc<Homes>)> {
    let homes = Arc::new(Homes::open(root).await?);
    let reading = Arc::clone(&homes);
    let images = move |request: Request| {
        let homes = Arc::clone(&reading);
        async move {
            match homes.reached(request.headers()).await {
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
    let reaching = Arc::clone(&homes);
    let app = endpoints::router()
        .layer(axum::middleware::from_fn(
            move |mut request: Request, next: Next| {
                let homes = Arc::clone(&reaching);
                async move {
                    match homes.reached(request.headers()).await {
                        Ok(home) => {
                            request.extensions_mut().insert(home);
                            next.run(request).await
                        }
                        Err(refused) => refused.into_response(),
                    }
                }
            },
        ))
        .route("/images/oci/v2", any(images.clone()))
        .route("/images/oci/v2/", any(images.clone()))
        .route("/images/oci/v2/{*rest}", any(images));
    Ok((app, homes))
}

/// Where a home's images are served as a registry.
const REGISTRY_PATH: &str = "/images/oci";

/// The one user over stdio: the client that spawned it.
const CLIENT: &str = "client";

/// The projects of a host that authenticates no one: [`DEFAULT_PROJECT`], always, and each
/// project with a root under `root`, everyone the owner of each. A project here is its root,
/// so forgetting one leaves it until the caller removes that.
pub struct OpenProjects {
    root: PathBuf,
}

impl OpenProjects {
    pub fn new(root: impl Into<PathBuf>) -> OpenProjects {
        OpenProjects { root: root.into() }
    }

    fn exists(&self, project: &str) -> bool {
        project == DEFAULT_PROJECT
            || project_root(&self.root, project).is_ok_and(|dir| dir.is_dir())
    }

    fn known(&self, project: &str) -> Result<(), ProjectError> {
        match self.exists(project) {
            true => Ok(()),
            false => Err(ProjectError::Unknown(project.to_owned())),
        }
    }
}

impl ProjectStore for OpenProjects {
    fn role<'a>(
        &'a self,
        _user: &'a str,
        project: &'a str,
    ) -> BoxFuture<'a, Result<Option<Role>, ProjectError>> {
        Box::pin(async move { Ok(self.exists(project).then_some(Role::Owner)) })
    }

    fn projects<'a>(
        &'a self,
        _user: &'a str,
    ) -> BoxFuture<'a, Result<Vec<(String, Role)>, ProjectError>> {
        Box::pin(async move {
            let mut names = project_names(&self.root)?;
            names.push(DEFAULT_PROJECT.to_owned());
            names.sort();
            names.dedup();
            Ok(names.into_iter().map(|name| (name, Role::Owner)).collect())
        })
    }

    fn create<'a>(
        &'a self,
        project: &'a str,
        _owner: &'a str,
    ) -> BoxFuture<'a, Result<(), ProjectError>> {
        Box::pin(async move {
            let dir = project_root(&self.root, project)?;
            if project == DEFAULT_PROJECT {
                return Err(ProjectError::AlreadyExists(project.to_owned()));
            }
            let parent = self.root.join("project");
            fs::create_dir_all(&parent)
                .map_err(|e| ProjectError::io(format!("making {}", parent.display()), e))?;
            // Made exclusively: two creates of one name cannot both win.
            match fs::create_dir(&dir) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    Err(ProjectError::AlreadyExists(project.to_owned()))
                }
                Err(e) => Err(ProjectError::io(format!("making {}", dir.display()), e)),
            }
        })
    }

    fn remove<'a>(&'a self, project: &'a str) -> BoxFuture<'a, Result<(), ProjectError>> {
        Box::pin(async move { self.known(project) })
    }

    fn members<'a>(
        &'a self,
        project: &'a str,
    ) -> BoxFuture<'a, Result<Vec<(String, Role)>, ProjectError>> {
        Box::pin(async move { self.known(project).map(|()| Vec::new()) })
    }

    fn set_member<'a>(
        &'a self,
        _project: &'a str,
        _user: &'a str,
        _role: Option<Role>,
    ) -> BoxFuture<'a, Result<(), ProjectError>> {
        Box::pin(async { Err(ProjectError::NoMembers) })
    }
}

/// Each project's home, made as a request first reaches it.
pub struct Homes {
    root: PathBuf,
    projects: OpenProjects,
    made: Mutex<HashMap<String, Arc<Home>>>,
}

impl Homes {
    /// The projects under `root`, the home of each one already there made now, so that its
    /// sandboxes left from before are swept as the server starts.
    pub async fn open(root: &Path) -> anyhow::Result<Homes> {
        let homes = Homes {
            root: root.to_owned(),
            projects: OpenProjects::new(root),
            made: Mutex::default(),
        };
        for (name, _) in homes.projects.projects(CLIENT).await? {
            homes.home(&name)?;
        }
        Ok(homes)
    }

    /// The home of the project `headers` name, or of `default` where they name none: `403` for
    /// a project not here.
    pub async fn reached(&self, headers: &HeaderMap) -> Result<Arc<Home>, Refused> {
        let project = match headers.get(PROJECT_HEADER) {
            None => DEFAULT_PROJECT,
            Some(named) => named.to_str().map_err(|_| {
                refused(
                    StatusCode::BAD_REQUEST,
                    "invalid",
                    "the virtx-project header is not a project's name",
                )
            })?,
        };
        let internal = |e: &dyn std::fmt::Display| {
            refused(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                format!("reaching project {project}: {e:#}"),
            )
        };
        match self.projects.role(CLIENT, project).await {
            Ok(Some(_)) => self.home(project).map_err(|e| internal(&e)),
            Ok(None) => Err(refused(
                StatusCode::FORBIDDEN,
                "forbidden",
                format!("no project here is named {project}"),
            )),
            Err(e) => Err(internal(&e)),
        }
    }

    /// Project `project`'s home, its images its own.
    fn home(&self, project: &str) -> anyhow::Result<Arc<Home>> {
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

    /// Close every sandbox of every home, as removing each does: blocks until each machine has
    /// stopped and synced what it wrote.
    pub fn close_all(&self) {
        let homes: Vec<_> = self.made.lock().unwrap().values().cloned().collect();
        for home in homes {
            home.close_all();
        }
    }
}

/// Move what this host kept before it kept projects, at `root` itself, into `default`'s root:
/// once, while `default` has none.
pub fn adopt_unprojected(root: &Path) -> anyhow::Result<()> {
    let default = project_root(root, DEFAULT_PROJECT)?;
    if default.exists() {
        return Ok(());
    }
    for kept in ["images", "snapshot", "sandbox", "volume"] {
        let from = root.join(kept);
        if !from.exists() {
            continue;
        }
        fs::create_dir_all(&default).with_context(|| format!("making {}", default.display()))?;
        match fs::rename(&from, default.join(kept)) {
            Ok(()) => {}
            // Another `virtx-stdio`, starting beside this one, moved it first.
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("moving {} into project default", from.display()));
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
        /// The root of `default`, the project the client reaches.
        home: PathBuf,
        /// The app the client is served, for a request no client sends.
        app: axum::Router,
    }

    impl Server {
        async fn spawn(name: &str) -> Server {
            Server::spawn_after(name, |_, _| {}).await
        }

        /// `spawn`, with `after` run once the app has answered each request, given `default`'s
        /// root and the request's path: for what another client does between a client's
        /// requests.
        async fn spawn_after(
            name: &str,
            after: impl Fn(&Path, &str) + Send + Sync + 'static,
        ) -> Server {
            let root =
                std::env::temp_dir().join(format!("virtx-stdio-app-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            let home = root.join("project").join("default");
            let (app, _) = app(&root).await.unwrap();
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

    #[tokio::test]
    async fn without_members_the_client_owns_default_and_each_project_made() {
        let root = std::env::temp_dir().join(format!("virtx-stdio-open-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let store = OpenProjects::new(&root);

        assert_eq!(
            store.role(CLIENT, DEFAULT_PROJECT).await.unwrap(),
            Some(Role::Owner)
        );
        assert_eq!(store.role(CLIENT, "demo").await.unwrap(), None);
        assert_eq!(store.role(CLIENT, "Demo").await.unwrap(), None);

        store.create("demo", CLIENT).await.unwrap();
        assert!(root.join("project/demo").is_dir());
        assert_eq!(store.role(CLIENT, "demo").await.unwrap(), Some(Role::Owner));
        assert!(matches!(
            store.create("demo", CLIENT).await,
            Err(ProjectError::AlreadyExists(_))
        ));
        assert!(matches!(
            store.create(DEFAULT_PROJECT, CLIENT).await,
            Err(ProjectError::AlreadyExists(_))
        ));
        assert_eq!(
            store.projects(CLIENT).await.unwrap(),
            [
                (DEFAULT_PROJECT.to_owned(), Role::Owner),
                ("demo".to_owned(), Role::Owner)
            ]
        );

        assert_eq!(store.members("demo").await.unwrap(), []);
        assert!(matches!(
            store.set_member("demo", "alice", Some(Role::Member)).await,
            Err(ProjectError::NoMembers)
        ));
        assert!(matches!(
            store.remove("nowhere").await,
            Err(ProjectError::Unknown(_))
        ));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn what_was_kept_before_projects_is_default_s() {
        let root = std::env::temp_dir().join(format!("virtx-stdio-adopt-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("volume/data/files")).unwrap();
        fs::create_dir_all(root.join("images/tag")).unwrap();
        fs::create_dir_all(root.join("virtx-stdio/0.1.0")).unwrap();

        adopt_unprojected(&root).unwrap();
        let default = root.join("project/default");
        assert!(default.join("volume/data/files").is_dir());
        assert!(default.join("images/tag").is_dir());
        assert!(!root.join("volume").exists() && !root.join("images").exists());
        // What is no project's stays where it was.
        assert!(root.join("virtx-stdio/0.1.0").is_dir());

        // Once only: what turns up at the root after is left there.
        fs::create_dir_all(root.join("volume")).unwrap();
        adopt_unprojected(&root).unwrap();
        assert!(root.join("volume").is_dir());
        let _ = fs::remove_dir_all(&root);
    }
}
