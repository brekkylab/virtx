//! `virtx-stdio` -- a virtx server over stdio: the endpoints `virtx-core`'s `PROTOCOL.md` names,
//! with the images as an OCI registry at `/images/oci/v2/` (see [`virtx_uvm::RegistryServer`]),
//! answered on stdin and stdout to the client that spawned it, until it closes stdin. That client
//! is the host's admin, who reaches every project here, each a directory of its own in the cache
//! (see [`virtx_uvm::ProjectStore`]): its own, `default`, and each one it made, by name, and those
//! a `virtx-server-tcp` over the same cache keeps for its users, by id.
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
use http::HeaderMap;
use tower::ServiceExt as _;
use virtx_core::{DEFAULT_PROJECT, PROJECT_HEADER, serve_stdio};
use virtx_uvm::{
    ImageStore, Project, ProjectError, ProjectId, ProjectOwner as Owner, ProjectStore,
    RegistryServer, Sandbox,
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

/// The projects this host keeps, every one the client's to reach, the home of each made as it
/// is first reached.
pub struct Host {
    store: ProjectStore,
    /// Each project's home, by id.
    /// `None` for a project being removed or removed, which no request reaches.
    homes: Mutex<HashMap<ProjectId, Option<Arc<Home>>>>,
}

impl Host {
    /// The projects under `root`, what was kept at `root` before moved into the client's own
    /// (see [`adopt_unprojected`]), and the home of each made now, so that its sandboxes left
    /// from before are swept as the server starts.
    async fn open(root: &Path) -> anyhow::Result<Host> {
        let host = Host {
            store: ProjectStore::new(root),
            homes: Mutex::default(),
        };
        let own = host.project(None)?.context("the client's own project")?;
        adopt_unprojected(root, &host.store.path(&own.id))?;
        for id in host.store.list()? {
            host.home_of(&id)?;
        }
        Ok(host)
    }

    pub fn store(&self) -> &ProjectStore {
        &self.store
    }

    /// The project `named` names, if any: the client's own, [`DEFAULT_PROJECT`], where it is
    /// `None`, made if it is not there; the project of that id, where it is one, whoever's it
    /// is; and otherwise the admin's project of that name.
    pub fn project(&self, named: Option<&str>) -> Result<Option<Project>, ProjectError> {
        let named = named.unwrap_or(DEFAULT_PROJECT);
        if let Ok(id) = named.parse::<ProjectId>() {
            return self.store.read(&id);
        }
        if let Some(project) = self.store.find(&Owner::Admin, named)? {
            return Ok(Some(project));
        }
        if named != DEFAULT_PROJECT {
            return Ok(None);
        }
        match self.create(DEFAULT_PROJECT) {
            Ok(project) => Ok(Some(project)),
            // Made by a `virtx-stdio` beside this one.
            Err(ProjectError::AlreadyExists(_)) => self.store.find(&Owner::Admin, named),
            Err(e) => Err(e),
        }
    }

    /// Every project here, whoever's, by name.
    pub fn projects(&self) -> Result<Vec<Project>, ProjectError> {
        let mut all = self.store.records()?;
        all.sort_by(|a, b| (&a.name, &a.owner).cmp(&(&b.name, &b.owner)));
        Ok(all)
    }

    /// Make the admin's project `name`: refused where the admin has one named so.
    pub fn create(&self, name: &str) -> Result<Project, ProjectError> {
        self.store.create(&Owner::Admin, name, BTreeMap::new())
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
            .map_err(endpoints::internal)?
            .ok_or_else(|| forbidden(format!("no project here is {named:?}")))?;
        let home = self.home_of(&project.id).map_err(|e| {
            endpoints::internal(format!("making project {}'s home: {e:#}", project.name))
        })?;
        home.ok_or_else(|| forbidden(format!("project {} is being removed", project.name)))
    }

    /// Project `id`'s home, its images its own: `None` once it is being removed.
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

    /// Take project `id`'s home away, closed and its image jobs cancelled, for it to be removed:
    /// no request reaches it from now on, or until [`release`](Host::release) where the removal
    /// fails. `false`, and nothing taken, while a sandbox lives or is being made in it, or it is
    /// being removed already.
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

/// Move what this host kept before it kept projects, at `root` itself, into `default`, the
/// directory of the client's own project: once, while it has none of it.
fn adopt_unprojected(root: &Path, default: &Path) -> anyhow::Result<()> {
    const KEPT: [&str; 3] = ["images", "sandbox", "volume"];
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
    //! the endpoints over a `VIRTX_HOME` of the test's own, and the files of its volumes among
    //! them.

    use std::{
        path::{Path, PathBuf},
        sync::Arc,
    };

    use virtx_core::{
        Client, DirEntry, Error, FileRoot, ImageName, MountSource, MountType, Recipe,
        SandboxCreateArgs, SandboxExecArgs, SandboxVolume,
    };
    use virtx_uvm::{FsTree, ImageStore};

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
            let own = host.project(None).unwrap().unwrap();
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
        let long = "x".repeat(129);
        for name in [
            "",
            ".dot",
            "-dash",
            "a b",
            "caf\u{e9}",
            "a/b",
            long.as_str(),
        ] {
            assert_eq!(refused(here().name(name)).await, 400, "name {name:?}");
        }
        let mut args = here();
        args.vcpus = Some(0);
        assert_eq!(refused(args).await, 400);
        for mib in [0, 16, 32] {
            let mut args = here();
            args.memory_mib = Some(mib);
            assert_eq!(refused(args).await, 400, "memory_mib {mib}");
        }
        for gib in [0, 1025, 16384, u32::MAX] {
            let mut args = here();
            args.disk_gib = Some(gib);
            assert_eq!(refused(args).await, 400, "disk_gib {gib}");
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
            Err(Error::Status(400, body, _)) => assert!(body.contains("given twice"), "{body}"),
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

    /// The client owns every project: its own, `default`, there from the start, and each it
    /// makes. A project's things are its own, and go with it.
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
            made,
            serde_json::json!({"id": id, "name": "demo", "role": "owner"})
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

        assert_eq!(
            status("GET", "/projects/demo/members/", None, "").await,
            404
        );
        let member = r#"{"role": "member"}"#;
        assert_eq!(
            status("PUT", "/projects/demo/members/alice", None, member).await,
            404
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

    /// The projects a `virtx-server-tcp` over the same cache keeps for its users are here too:
    /// listed with their owners, and reached by id, the client's names its own alone.
    #[tokio::test]
    async fn a_user_s_projects_over_the_same_cache_are_reached_by_id() {
        let server = Server::spawn("users-projects").await;
        let alice = virtx_uvm::ProjectOwner::User("alice".into());
        let members = BTreeMap::from([("alice".to_owned(), virtx_core::Role::Owner)]);
        let store = ProjectStore::new(&server.root);
        let theirs = store
            .create(&alice, DEFAULT_PROJECT, members.clone())
            .unwrap();
        let demo = store.create(&alice, "demo", members).unwrap();

        let listed: Vec<_> = server
            .client
            .list_project()
            .await
            .unwrap()
            .into_iter()
            .map(|project| (project.name, project.owner, project.role))
            .collect();
        let alice_s = Some("alice".to_owned());
        assert_eq!(
            listed,
            [
                ("default".to_owned(), None, virtx_core::Role::Owner),
                (
                    "default".to_owned(),
                    alice_s.clone(),
                    virtx_core::Role::Owner
                ),
                ("demo".to_owned(), alice_s.clone(), virtx_core::Role::Owner),
            ]
        );
        assert_ne!(
            server.client.get_project("default").await.unwrap().id,
            theirs.id.to_string()
        );
        assert_eq!(status(server.client.get_project("demo").await), 403);
        let by_id = server
            .client
            .get_project(demo.id.to_string())
            .await
            .unwrap();
        assert_eq!((by_id.name, by_id.owner), ("demo".to_owned(), alice_s));
    }

    /// A project is closed for its removal only while no sandbox of this process or another lives
    /// in it; closed, a request already in its home starts nothing, and removed, it gets no home
    /// again.
    #[tokio::test]
    async fn a_project_is_closed_only_while_no_sandbox_lives_in_it() {
        let root = std::env::temp_dir().join(format!("virtx-stdio-close-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let host = Host::open(&root).await.unwrap();
        let demo = host.create("demo").unwrap().id;
        let unreached = host.create("unreached").unwrap().id;
        let home = host.home_of(&demo).unwrap().unwrap();

        // An upper disk another process's sandbox holds, in a project with a home here or not.
        for id in [demo, unreached] {
            let sandbox = host.store().path(&id).join("sandbox");
            fs::create_dir_all(&sandbox).unwrap();
            let held = fs::File::create(sandbox.join("other.ext4.lock")).unwrap();
            held.lock().unwrap();
            assert!(!host.close(&id));
            drop(held);
        }

        assert!(host.close(&demo));
        let pull = http::Request::post("/images/jobs/pull/")
            .header(http::header::CONTENT_TYPE, "application/json")
            .body(axum::body::Body::from(r#"{"reference": "alpine:3.20"}"#))
            .unwrap();
        let in_home = crate::endpoints::router().layer(axum::Extension(home));
        let Ok(refused) = in_home.oneshot(pull).await;
        assert_eq!(refused.status(), http::StatusCode::FORBIDDEN);
        assert!(!host.close(&demo));
        assert!(host.home_of(&demo).unwrap().is_none());
        let _ = fs::remove_dir_all(&root);
    }

    /// A removal forgets the project only once it holds the lock a record changes under, which
    /// a server beside this one holds as it changes one: held from here, the removal is not
    /// answered and the project is there; let go, it is removed.
    #[tokio::test]
    async fn a_project_is_forgotten_only_under_the_records_lock() {
        let server = Server::spawn("forget-locked").await;
        server.client.create_project("demo").await.unwrap();
        let store = ProjectStore::new(&server.root);
        let lock = store.lock().unwrap();

        let (app, _) = app(&server.root).await.unwrap();
        let request = http::Request::delete("/projects/demo")
            .body(axum::body::Body::empty())
            .unwrap();
        let removed = tokio::spawn(async move {
            let Ok(response) = app.oneshot(request).await;
            response.status().as_u16()
        });
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(
            !removed.is_finished(),
            "it was answered while the lock was held"
        );
        assert!(store.find(&Owner::Admin, "demo").unwrap().is_some());

        drop(lock);
        assert_eq!(removed.await.unwrap(), 204);
        assert_eq!(store.find(&Owner::Admin, "demo").unwrap(), None);
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

    /// A file's `getetag`, as a `PROPFIND` answers it, is the entity-tag its `ETag` header is,
    /// quotes and all, so a client that sends it as `If-Match` writes over the file it read: the
    /// write lands, and the same tag again, gone stale, is refused `412`.
    #[tokio::test]
    async fn a_file_s_getetag_is_its_etag_and_is_taken_as_if_match() {
        use axum::body::Body;
        use http::header::{ETAG, IF_MATCH};

        let server = Server::spawn("getetag").await;
        server.client.create_volume(Some("data")).await.unwrap();
        let app = server.app.clone();
        let path = "/volumes/data/files/a.txt";
        let send = |request: http::Request<Body>| {
            let app = app.clone();
            async move {
                let Ok(response) = app.oneshot(request).await;
                let status = response.status().as_u16();
                let etag = response
                    .headers()
                    .get(ETAG)
                    .map(|etag| etag.to_str().unwrap().to_owned());
                let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                    .await
                    .unwrap();
                (status, etag, String::from_utf8(body.to_vec()).unwrap())
            }
        };
        let put = |if_match: Option<&str>, body: &'static str| {
            let mut request = http::Request::put(path);
            if let Some(tag) = if_match {
                request = request.header(IF_MATCH, tag);
            }
            request.body(Body::from(body)).unwrap()
        };
        let getetag = async || {
            let propfind = http::Request::builder()
                .method("PROPFIND")
                .uri(path)
                .header("Depth", "0")
                .body(Body::empty())
                .unwrap();
            let (status, _, xml) = send(propfind).await;
            assert_eq!(status, 207, "{xml}");
            let start = xml.find("getetag>").expect("a getetag") + "getetag>".len();
            xml[start..start + xml[start..].find('<').unwrap()].to_owned()
        };

        assert_eq!(send(put(None, "one")).await.0, 201);
        let head = http::Request::head(path).body(Body::empty()).unwrap();
        let header = send(head).await.1.expect("an ETag");
        let tag = getetag().await;
        assert_eq!(tag, header, "the getetag and the ETag header");

        assert_eq!(send(put(Some(&tag), "three")).await.0, 204);
        assert_eq!(send(put(Some(&tag), "stale")).await.0, 412);
    }

    /// A `PROPPATCH`, which WebDAV class 1 asks a server for, is answered `207` with the status
    /// of each property it names, and leaves the connection open.
    #[tokio::test]
    async fn a_proppatch_is_answered_with_each_property_s_status() {
        use axum::body::Body;

        let server = Server::spawn("proppatch").await;
        server.client.create_volume(Some("data")).await.unwrap();
        let volume = FileRoot::Volume("data".into());
        server
            .client
            .write_file(&volume, "a.txt", "a", None)
            .await
            .unwrap();
        let proppatch = http::Request::builder()
            .method("PROPPATCH")
            .uri("/volumes/data/files/a.txt")
            .header(http::header::CONTENT_TYPE, "application/xml")
            .body(Body::from(
                r#"<?xml version="1.0"?><D:propertyupdate xmlns:D="DAV:"><D:set><D:prop><D:displayname>a</D:displayname></D:prop></D:set></D:propertyupdate>"#,
            ))
            .unwrap();
        let Ok(response) = server.app.clone().oneshot(proppatch).await;
        assert_eq!(response.status(), 207);
        assert!(response.headers().get(http::header::CONNECTION).is_none());
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let xml = String::from_utf8(body.to_vec()).unwrap();
        assert!(xml.contains("displayname"), "{xml}");
    }

    /// Two conditional writes to one file at once, from two apps over one home as two server
    /// processes are, each with its own requests in memory: the second, sent while the first is
    /// still sending its body, is not held up by it and lands, and the first, checked once its
    /// body has arrived, is refused `412` over a file no longer the one its condition named. For
    /// `If-None-Match: *` to a new file, and for one `If-Match` to a file there.
    #[tokio::test]
    async fn of_two_conditional_writes_to_one_file_from_two_servers_one_lands() {
        use axum::body::{Body, Bytes};
        use futures_util::stream;
        use http::header;
        use tokio::sync::oneshot;

        let server = Server::spawn("conditional-writes").await;
        let client = &server.client;
        client.create_volume(Some("data")).await.unwrap();
        let volume = FileRoot::Volume("data".into());
        client
            .write_file(&volume, "there", "there", None)
            .await
            .unwrap();
        let etag = client
            .stat_file(&volume, "there")
            .await
            .unwrap()
            .etag
            .unwrap();
        let (one, _) = app(&server.root).await.unwrap();
        let (two, _) = app(&server.root).await.unwrap();

        for (path, condition, value) in [
            ("new", header::IF_NONE_MATCH, "*".to_owned()),
            ("there", header::IF_MATCH, etag),
        ] {
            let put = |app: axum::Router, body: Body| {
                let request = http::Request::put(format!("/volumes/data/files/{path}"))
                    .header(condition.clone(), &value)
                    .body(body)
                    .unwrap();
                async move {
                    let Ok(response) = app.oneshot(request).await;
                    response.status().as_u16()
                }
            };
            // The first's body: a part, then the rest once let go. Once its first part is
            // read the first is still sending, and the second goes meanwhile.
            let (sending, read) = oneshot::channel();
            let (go, let_go) = oneshot::channel::<()>();
            let slow = stream::unfold((Some(sending), Some(let_go)), |state| async move {
                match state {
                    (Some(sending), let_go) => {
                        let _ = sending.send(());
                        Some((
                            Ok::<_, std::io::Error>(Bytes::from("first, ")),
                            (None, let_go),
                        ))
                    }
                    (None, Some(let_go)) => {
                        let _ = let_go.await;
                        Some((Ok(Bytes::from("whole")), (None, None)))
                    }
                    (None, None) => None,
                }
            });
            let first = tokio::spawn(put(one.clone(), Body::from_stream(slow)));
            read.await.unwrap();
            let second = put(two.clone(), Body::from("second")).await;
            assert!(
                matches!(second, 201 | 204),
                "{path}: the second answered {second}"
            );
            go.send(()).unwrap();
            assert_eq!(first.await.unwrap(), 412, "{path}");
            assert_eq!(client.read_text(&volume, path).await.unwrap(), "second");
        }
    }

    /// A conditional write to a volume lands only once it holds the volume's `writing` file,
    /// which another server over the home holds as its own lands: held from here, the write's
    /// body is all in and it has not landed, nor been answered; let go, it lands.
    #[tokio::test]
    async fn a_conditional_write_lands_only_once_it_holds_its_volume() {
        let server = Server::spawn("conditional-held").await;
        let client = &server.client;
        client.create_volume(Some("data")).await.unwrap();
        let volume = FileRoot::Volume("data".into());
        let dir = server.home.join("volume").join("data");
        assert!(
            dir.join("files").is_dir(),
            "{} is not the volume",
            dir.display()
        );
        let writing = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join("writing"))
            .unwrap();
        writing.lock().unwrap();

        let (app, _) = app(&server.root).await.unwrap();
        let request = http::Request::put("/volumes/data/files/new")
            .header(http::header::IF_NONE_MATCH, "*")
            .body(axum::body::Body::from("held"))
            .unwrap();
        let put = tokio::spawn(async move {
            let Ok(response) = app.oneshot(request).await;
            response.status().as_u16()
        });
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(
            !put.is_finished(),
            "it was answered while the volume was held"
        );
        assert!(
            !dir.join("files").join("new").exists(),
            "it landed while the volume was held"
        );
        assert_eq!(
            saving(&dir.join("files")).len(),
            1,
            "its body is not beside the path while the volume is held"
        );

        writing.unlock().unwrap();
        assert_eq!(put.await.unwrap(), 201);
        assert_eq!(client.read_text(&volume, "new").await.unwrap(), "held");
        assert_eq!(saving(&dir.join("files")), Vec::<String>::new());
    }

    /// A conditional write whose condition does not hold is refused `412`, and leaves nothing
    /// behind: over a file whose etag has gone stale, over a directory, and under a directory
    /// that is not there; with `If-Match` over the file, but a WebDAV `If` or an
    /// `If-Unmodified-Since` that does not hold; and with `If-None-Match` naming the file by its
    /// tag marked weak, which names it all the same. One to a path with a `..` is refused `400`,
    /// as any request is.
    #[tokio::test]
    async fn a_conditional_write_whose_condition_fails_is_refused_and_leaves_nothing() {
        use http::header::{IF_MATCH, IF_NONE_MATCH, IF_UNMODIFIED_SINCE};

        let server = Server::spawn("conditional-refused").await;
        let client = &server.client;
        client.create_volume(Some("data")).await.unwrap();
        let volume = FileRoot::Volume("data".into());
        client
            .write_file(&volume, "there", "there", None)
            .await
            .unwrap();
        let etag = client
            .stat_file(&volume, "there")
            .await
            .unwrap()
            .etag
            .unwrap();
        client.create_dir(&volume, "dir").await.unwrap();
        let (app, _) = app(&server.root).await.unwrap();

        let long_ago = "Mon, 01 Jan 2001 00:00:00 GMT";
        let weak = format!("W/{etag}");
        let if_ = http::HeaderName::from_static("if");
        for (path, conditions) in [
            ("there", vec![(IF_MATCH, "\"stale\"")]),
            ("dir", vec![(IF_NONE_MATCH, "*")]),
            ("dir", vec![(IF_MATCH, "\"stale\"")]),
            ("none/file", vec![(IF_MATCH, "*")]),
            (
                "there",
                vec![(IF_MATCH, etag.as_str()), (if_, "([\"stale\"])")],
            ),
            (
                "there",
                vec![
                    (IF_NONE_MATCH, "\"other\""),
                    (IF_UNMODIFIED_SINCE, long_ago),
                ],
            ),
            ("there", vec![(IF_NONE_MATCH, weak.as_str())]),
        ] {
            let mut request = http::Request::put(format!("/volumes/data/files/{path}"));
            for (condition, value) in &conditions {
                request = request.header(condition, *value);
            }
            let request = request.body(axum::body::Body::from("written")).unwrap();
            let Ok(response) = app.clone().oneshot(request).await;
            assert_eq!(response.status(), 412, "{path} with {conditions:?}");
        }
        // A path with a `..` in it is refused before any of it is read, as any request's is.
        let request = http::Request::put("/volumes/data/files/dir/../there")
            .header(IF_MATCH, "*")
            .body(axum::body::Body::from("written"))
            .unwrap();
        let Ok(response) = app.clone().oneshot(request).await;
        assert_eq!(response.status(), 400);
        assert_eq!(client.read_text(&volume, "there").await.unwrap(), "there");
        let files = server.home.join("volume").join("data").join("files");
        assert_eq!(saving(&files), Vec::<String>::new());
        assert_eq!(saving(&files.join("dir")), Vec::<String>::new());
    }

    /// The names in `dir` of writes on their way, beside the paths they are going to.
    fn saving(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with('.') && name.ends_with(".saving"))
            .collect()
    }

    /// A refusal made before a handler runs is in PROTOCOL.md's envelope too, where a client
    /// reads its code: a body over the server's limit is `413` `too_large`, naming the limit,
    /// and a path whose segment does not read as one is `400` `invalid`. A file's body has no
    /// such limit.
    #[tokio::test]
    async fn a_body_over_the_limit_and_a_path_that_does_not_read_are_refused_as_any_refusal_is() {
        let server = Server::spawn("envelope").await;
        let (app, _) = app(&server.root).await.unwrap();
        let refusal = |response: http::Response<axum::body::Body>| async move {
            let status = response.status().as_u16();
            let kind = response.headers()[http::header::CONTENT_TYPE].clone();
            let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
            (status, kind, body["error"].clone())
        };

        let over = http::Request::post("/sandboxes/")
            .header(http::header::CONTENT_TYPE, "application/json")
            .body(axum::body::Body::from(vec![b' '; 3 << 20]))
            .unwrap();
        let Ok(response) = app.clone().oneshot(over).await;
        let (status, kind, error) = refusal(response).await;
        assert_eq!((status, kind.to_str().unwrap()), (413, "application/json"));
        assert_eq!(error["code"], "too_large", "{error}");
        assert!(
            error["message"].as_str().unwrap().contains("2 MiB"),
            "{error}"
        );

        let unreadable = http::Request::get("/sandboxes/%FF")
            .body(axum::body::Body::empty())
            .unwrap();
        let Ok(response) = app.clone().oneshot(unreadable).await;
        let (status, kind, error) = refusal(response).await;
        assert_eq!((status, kind.to_str().unwrap()), (400, "application/json"));
        assert_eq!(error["code"], "invalid", "{error}");

        // A file's body is streamed, and has no limit.
        server.client.create_volume(Some("data")).await.unwrap();
        let file = http::Request::put("/volumes/data/files/big")
            .body(axum::body::Body::from(vec![b'x'; 3 << 20]))
            .unwrap();
        let Ok(response) = app.clone().oneshot(file).await;
        assert_eq!(response.status(), 201);
    }

    /// A body or a path that does not read is refused `400` `invalid` in the server's words:
    /// the member or segment, what it takes, and what was sent.
    #[tokio::test]
    async fn a_body_or_a_path_that_does_not_read_is_refused_naming_what_it_takes() {
        let server = Server::spawn("unread").await;
        let (app, _) = app(&server.root).await.unwrap();
        let image = r#""image": {"type": "tag", "tag": "alpine"}"#;
        let ports = format!(r#"{{{image}, "ports": ["5900"]}}"#);
        for (method, path, body, said) in [
            (
                "POST",
                "/sandboxes/nobody/jobs/exec/",
                "not json",
                "the body: JSON, which it stops being at line 1, column 2",
            ),
            (
                "POST",
                "/sandboxes/nobody/jobs/exec/",
                "[]",
                "the body: an object, not a list",
            ),
            (
                "POST",
                "/sandboxes/nobody/jobs/exec/",
                "",
                "the body: a JSON object, and none was sent",
            ),
            (
                "POST",
                "/sandboxes/nobody/jobs/exec/",
                r#"{"cmd": "ls"}"#,
                r#"cmd: a list, not "ls""#,
            ),
            (
                "POST",
                "/sandboxes/nobody/jobs/j/signal",
                r#"{"signl": "TERM"}"#,
                "signl: not a member; the members are `signal`",
            ),
            (
                "POST",
                "/images/jobs/pull/",
                "x",
                "the body: JSON, which it stops being at line 1, column 1",
            ),
            (
                "POST",
                "/sandboxes/",
                &ports,
                r#"ports[0]: a whole number from 0 to 65535, not "5900""#,
            ),
            (
                "GET",
                "/sandboxes/%FF",
                "",
                "id: UTF-8 once its escapes are read, and it is not",
            ),
        ] {
            let request = http::Request::builder()
                .method(method)
                .uri(path)
                .header(http::header::CONTENT_TYPE, "application/json")
                .body(axum::body::Body::from(body.to_owned()))
                .unwrap();
            let Ok(response) = app.clone().oneshot(request).await;
            let how = format!("{method} {path} {body}");
            assert_eq!(response.status(), 400, "{how}");
            let answer = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            let answer: serde_json::Value = serde_json::from_slice(&answer).unwrap();
            assert_eq!(answer["error"]["code"], "invalid", "{how}");
            assert_eq!(answer["error"]["message"], said, "{how}");
        }
    }

    /// Each route that takes a JSON body holds it to the limit: one a byte over is refused `413`
    /// `too_large`, one at the limit is not, and one that breaks off is `400` `invalid`, each in
    /// the envelope. The body is refused before the handler looks anything up, so no sandbox,
    /// volume or job has to be there.
    #[tokio::test]
    async fn every_route_that_takes_a_body_holds_it_to_the_limit() {
        use axum::body::{Body, Bytes};

        let server = Server::spawn("limit").await;
        let (app, _) = app(&server.root).await.unwrap();
        let answer = |method: &'static str, path: &'static str, body: Body| {
            let app = app.clone();
            async move {
                let request = http::Request::builder()
                    .method(method)
                    .uri(path)
                    .header(http::header::CONTENT_TYPE, "application/json")
                    .body(body)
                    .unwrap();
                let Ok(response) = app.oneshot(request).await;
                let status = response.status().as_u16();
                let kind = response.headers()[http::header::CONTENT_TYPE].clone();
                let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                    .await
                    .unwrap();
                let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
                (status, kind, body["error"]["code"].clone())
            }
        };
        let limit = 2 << 20;
        for (method, path) in [
            ("POST", "/images/jobs/pull/"),
            ("POST", "/images/jobs/push/"),
            ("POST", "/images/jobs/build/"),
            ("POST", "/volumes/"),
            ("PUT", "/volumes/nobody/mounts/m"),
            ("POST", "/sandboxes/"),
            ("POST", "/sandboxes/nobody/jobs/exec/"),
            ("POST", "/sandboxes/nobody/jobs/j/signal"),
        ] {
            let over = Body::from(vec![b' '; limit + 1]);
            let (status, kind, code) = answer(method, path, over).await;
            assert_eq!(
                (status, code.as_str()),
                (413, Some("too_large")),
                "{method} {path}"
            );
            assert_eq!(kind, "application/json", "{method} {path}");
            let at = Body::from(vec![b' '; limit]);
            let (status, kind, code) = answer(method, path, at).await;
            assert_eq!(
                (status, code.as_str()),
                (400, Some("invalid")),
                "{method} {path}"
            );
            assert_eq!(kind, "application/json", "{method} {path}");
        }

        let broken = futures_util::stream::iter([
            Ok(Bytes::from_static(b"{")),
            Err(std::io::Error::other("the client went")),
        ]);
        let (status, _, code) = answer("POST", "/sandboxes/", Body::from_stream(broken)).await;
        assert_eq!((status, code.as_str()), (400, Some("invalid")));
    }

    /// A body over the limit is read to its end before it is refused, though only what fits the
    /// limit is kept: answered with it unread, a client on a connection still sending it would
    /// see the connection reset, not the `413`.
    #[tokio::test]
    async fn a_body_over_the_limit_is_read_to_its_end_before_it_is_refused() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        use futures_util::StreamExt as _;

        let server = Server::spawn("read-to-end").await;
        let (app, _) = app(&server.root).await.unwrap();
        let read = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&read);
        let chunks =
            (0..48).map(|_| Ok::<_, std::io::Error>(axum::body::Bytes::from(vec![b' '; 64 << 10])));
        let stream = futures_util::stream::iter(chunks).inspect(move |chunk| {
            if let Ok(chunk) = chunk {
                counted.fetch_add(chunk.len(), Ordering::SeqCst);
            }
        });
        let request = http::Request::post("/sandboxes/")
            .header(http::header::CONTENT_TYPE, "application/json")
            .body(axum::body::Body::from_stream(stream))
            .unwrap();
        let Ok(response) = app.clone().oneshot(request).await;
        assert_eq!(response.status().as_u16(), 413);
        assert_eq!(read.load(Ordering::SeqCst), 48 * (64 << 10));
    }
}
