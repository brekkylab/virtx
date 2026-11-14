//! `virtx-stdio` -- a virtx server over stdio: the endpoints `virtx-core`'s `PROTOCOL.md` names,
//! with the images as an OCI registry at `/images/oci/v2/` (see [`virtx_uvm::RegistryServer`]),
//! answered on stdin and stdout to the client that spawned it, until it closes stdin. Everything
//! this host keeps is that one client's.
//!
//! `virtx-stdio vm …` is this binary spawned by itself to be a machine; see
//! [`virtx_uvm::Machine`].

mod endpoints;

use std::sync::Arc;

use axum::Extension;
use axum::routing::any;
use tower::ServiceExt as _;
use virtx_core::serve_stdio;
use virtx_uvm::{ImageStore, RegistryServer};

use crate::endpoints::Home;

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
    let (app, home) = app(&virtx_uvm::cache_root())?;
    home.tend();
    let handle = move |request: http::Request<_>| {
        let app = app.clone();
        async move {
            let Ok(response) = app.oneshot(request).await;
            response
        }
    };
    let served = serve_stdio(handle, tokio::io::stdin(), tokio::io::stdout()).await;
    // The client is gone: every sandbox stops, kept for the next client.
    tokio::task::spawn_blocking(move || home.close_all()).await?;
    Ok(served?)
}

/// Everything served over `root`: the endpoints, from its home, and the images' registry.
fn app(root: &std::path::Path) -> anyhow::Result<(axum::Router, Arc<Home>)> {
    let images = ImageStore::new(root.join("images"))?;
    let home = Arc::new(Home::new(images.clone(), root)?);
    let registry = Arc::new(RegistryServer::new(images, "/images/oci")?);
    let images = move |request: axum::extract::Request| {
        let registry = Arc::clone(&registry);
        async move { registry.handle(request).await.map(axum::body::Body::new) }
    };
    let app = endpoints::router()
        .layer(Extension(Arc::clone(&home)))
        .route("/images/oci/v2", any(images.clone()))
        .route("/images/oci/v2/", any(images.clone()))
        .route("/images/oci/v2/{*rest}", any(images));
    Ok((app, home))
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
        Client, DirEntry, Error, FileRoot, ImageName, InboundRule, MountSource, MountType,
        PortSpec, Recipe, SandboxCreateArgs, SandboxExecArgs, SandboxPortOpenArgs, SandboxVolume,
    };
    use virtx_uvm::{FsTree, ImageStore};

    use super::*;

    /// The app over a home of its own, served as over stdio, and a client of it; the home is
    /// removed when this drops.
    struct Server {
        client: Client,
        home: PathBuf,
        /// The app the client is served, for a request no client sends.
        app: axum::Router,
    }

    impl Server {
        fn spawn(name: &str) -> Server {
            Server::spawn_after(name, |_, _| {})
        }

        /// `spawn`, with `after` run once the app has answered each request, given the home
        /// and the request's path: for what another client does between a client's requests.
        fn spawn_after(name: &str, after: impl Fn(&Path, &str) + Send + Sync + 'static) -> Server {
            let home =
                std::env::temp_dir().join(format!("virtx-stdio-app-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&home);
            std::fs::create_dir_all(&home).unwrap();
            let (app, _) = app(&home).unwrap();
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
            let _ = std::fs::remove_dir_all(&self.home);
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
        let server = Server::spawn("build-refused");
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
        let server = Server::spawn("volume");
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
        let server = Server::spawn("mount");
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
        let server = Server::spawn("sandbox-refused");
        let digest = server.file_image("here:latest");
        let refused =
            async |args: SandboxCreateArgs| status(server.client.create_sandbox(&args).await);
        let here = || SandboxCreateArgs::new("here:latest");

        for ports in [vec![0], vec![8080, 8080]] {
            let mut args = here();
            args.ports = ports.into_iter().map(Into::into).collect();
            assert_eq!(refused(args).await, 400);
        }
        let mut args = here();
        args.vcpus = Some(0);
        assert_eq!(refused(args).await, 400);
        // Inbound rules are virtx cloud's: refused as the protocol refuses what is invalid.
        let mut ssh = PortSpec::new(22);
        ssh.inbound = Some(vec![InboundRule::new("10.0.0.0/8")]);
        let mut args = here();
        args.ports = vec![ssh];
        let error = server.client.create_sandbox(&args).await.unwrap_err();
        assert_eq!((error.status(), error.code()), (Some(400), Some("invalid")));
        assert!(error.to_string().contains("virtx cloud's"), "{error}");
        let open = SandboxPortOpenArgs {
            inbound: Some(Vec::new()),
            ..Default::default()
        };
        let opened = server.client.open_sandbox_port_with("nobody", 22, &open);
        assert_eq!(status(opened.await), 400);
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
        args.disk_gib = Some(0);
        assert_eq!(refused(args).await, 400);
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
        let missing = exec(&["true"]).await.unwrap_err();
        assert_eq!(
            (missing.status(), missing.code()),
            (Some(404), Some("not_found"))
        );
        assert_eq!(status(exec(&[]).await), 400);
        assert_eq!(status(server.client.remove_sandbox("nobody").await), 404);
    }

    #[tokio::test]
    async fn the_files_of_a_volume_are_written_listed_moved_and_read_back() {
        let server = Server::spawn("volume-files");
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
        let server = Server::spawn("registry");
        let digest = server.file_image("alpine:3.21");
        let listed = server.client.list_image().await.unwrap();
        assert_eq!(listed.len(), 1, "{listed:?}");
        assert_eq!(listed[0].digest, digest);
    }

    /// A list is three requests: the catalog, each repository's tags, each tag's manifest.
    /// Between the first two, another client removes a repository's only tag.
    #[tokio::test]
    async fn a_repository_removed_mid_list_is_left_out() {
        let server = Server::spawn_after("list-race", |home, path| {
            if path == "/images/oci/v2/_catalog" {
                let store = ImageStore::new(home.join("images")).unwrap();
                store.remove_tag(&"gone:latest".parse().unwrap()).unwrap();
            }
        });
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

    /// A prune takes an image no tag names once it is old, and what only it held, and leaves a
    /// tagged one.
    #[tokio::test]
    async fn a_prune_takes_an_old_untagged_image() {
        let server = Server::spawn("prune");
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
}
