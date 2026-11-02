//! The built binary, called as a client calls it: spawned, and spoken to on its stdin and
//! stdout, over a `VIRTX_HOME` of its own -- the RPC, and the files of its volumes and
//! snapshots beside it.

use std::path::PathBuf;
use std::pin::pin;

use futures_util::StreamExt as _;
use tokio::io::AsyncReadExt as _;
use tokio::net::TcpStream;
use tokio::process::Command;
use virtx_core::{
    DirEntry, Error, Event, FileRoot, ImageBuild, ImageBuildArgs, ImageBuildOutput, ImageName,
    ImagePrune, ImagePruneArgs, MountSource, MountType, Recipe, Request, SandboxClose,
    SandboxCloseArgs, SandboxExec, SandboxExecArgs, SandboxInit, SandboxInitArgs, SandboxList,
    SandboxListArgs, SandboxVolume, SnapshotCreate, SnapshotCreateArgs, SnapshotList,
    SnapshotListArgs, SnapshotRemove, SnapshotRemoveArgs, Step, TransportClient, VolumeCreate,
    VolumeCreateArgs, VolumeList, VolumeListArgs, VolumeMountCreate, VolumeMountCreateArgs,
    VolumeMountList, VolumeMountListArgs, VolumeMountRemove, VolumeMountRemoveArgs, VolumeRemove,
    VolumeRemoveArgs,
};
use virtx_uvm::{Entry, FsTree, ImageStore, RegistryClient};

const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const NOT_FOUND: i64 = -31001;
const ALREADY_EXISTS: i64 = -31002;
const IN_USE: i64 = -31003;
const BUILD_FAILED: i64 = -31006;
const TIMED_OUT: i64 = -31007;

/// A server over an empty `VIRTX_HOME`, removed when this drops.
struct Server {
    client: TransportClient,
    home: PathBuf,
}

impl Server {
    fn spawn(name: &str) -> Server {
        let home = std::env::temp_dir().join(format!("virtx-stdio-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_virtx-uvm"));
        command.env("VIRTX_HOME", &home);
        Server {
            client: TransportClient::spawn(command).unwrap(),
            home,
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

    /// Pull `reference` from its registry into the store the server reads, under its own name.
    async fn pull_image(&self, reference: &str) {
        let store = ImageStore::new(self.home.join("images")).unwrap();
        let reference = reference.parse().unwrap();
        let registry = RegistryClient::new(store.clone(), None);
        let (_, _, manifest) = registry.pull_manifest(&reference).await.unwrap();
        let (_, config) = registry.pull_config(&reference, &manifest).await.unwrap();
        let mut layers = Vec::new();
        for descriptor in manifest.layers() {
            let tree = registry.pull_layer(&reference, descriptor).await.unwrap();
            layers.push(store.insert_layer(&tree).unwrap());
        }
        let image = store.insert_image(&config, &layers).unwrap();
        store.insert_tag(&reference, &image).unwrap();
    }

    async fn call<R: Request>(&self, args: R::Args) -> Result<R::Output, Error>
    where
        R::Output: Send,
    {
        self.client.request(R::NAME, args).await
    }

    /// What `cmd` wrote in `sandbox`, a line at a time, and how it ended.
    async fn exec(
        &self,
        sandbox: &str,
        cmd: &[&str],
        timeout_ms: Option<u64>,
    ) -> (Vec<String>, Result<i32, Error>) {
        let args = SandboxExecArgs {
            sandbox: sandbox.into(),
            cmd: cmd.iter().map(|arg| arg.to_string()).collect(),
            timeout_ms,
        };
        let mut events = pin!(
            self.client
                .request_with_progress::<virtx_core::SandboxExecOutput, String>(
                    SandboxExec::NAME,
                    args
                )
        );
        let mut lines = Vec::new();
        while let Some(event) = events.next().await {
            match event {
                Ok(Event::Progress(progress)) => lines.extend(progress.content),
                Ok(Event::Output(output)) => return (lines, Ok(output.code)),
                Err(e) => return (lines, Err(e)),
            }
        }
        unreachable!("a call's events end with its output or its error")
    }

    /// What building `recipe` as `reference` logged, a line at a time, and how it ended.
    async fn build(
        &self,
        recipe: Recipe,
        reference: &str,
    ) -> (Vec<String>, Result<ImageBuildOutput, Error>) {
        let args = ImageBuildArgs {
            recipe,
            reference: reference.into(),
        };
        let mut events = pin!(
            self.client
                .request_with_progress::<ImageBuildOutput, String>(ImageBuild::NAME, args)
        );
        let mut lines = Vec::new();
        while let Some(event) = events.next().await {
            match event {
                Ok(Event::Progress(progress)) => lines.extend(progress.content),
                Ok(Event::Output(output)) => return (lines, Ok(output)),
                Err(e) => return (lines, Err(e)),
            }
        }
        unreachable!("a call's events end with its output or its error")
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

fn code<T: std::fmt::Debug>(answered: Result<T, Error>) -> i64 {
    match answered {
        Err(Error::Call(error)) => error.code,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

/// The HTTP status a files request was refused with.
fn status<T: std::fmt::Debug>(answered: Result<T, Error>) -> u16 {
    match answered {
        Err(Error::Status(status, _)) => status,
        other => panic!("expected a status, got {other:?}"),
    }
}

fn names(entries: Vec<DirEntry>) -> Vec<String> {
    entries.into_iter().map(|entry| entry.name).collect()
}

/// A snapshot held by this process, as a machine over it would hold it.
fn hold(server: &Server, snapshot: &str) -> std::fs::File {
    let lock =
        std::fs::File::create(server.home.join(format!("snapshot/{snapshot}.ext4.lock"))).unwrap();
    lock.try_lock().unwrap();
    lock
}

#[tokio::test]
async fn an_unknown_method_is_refused_as_one() {
    let server = Server::spawn("unknown");
    let answered = server
        .client
        .request::<serde_json::Value>("image.nope", SnapshotListArgs {})
        .await;
    assert_eq!(code(answered), METHOD_NOT_FOUND);
}

#[tokio::test]
async fn what_cannot_be_built_is_refused_before_it_boots() {
    let server = Server::spawn("build-refused");
    let (_, built) = server
        .build(Recipe::new("alpine:3.20"), "not a reference")
        .await;
    assert_eq!(code(built), INVALID_PARAMS);
    let (_, built) = server
        .build(Recipe::new("Not A Base!"), "built:latest")
        .await;
    assert_eq!(code(built), INVALID_PARAMS);
}

#[tokio::test]
async fn creates_lists_and_removes_a_snapshot() {
    let server = Server::spawn("snapshot");
    let created = server
        .call::<SnapshotCreate>(SnapshotCreateArgs {
            name: Some("work".into()),
            disk_gib: Some(1),
        })
        .await
        .unwrap();
    assert_eq!(created.name, "work");

    let listed = server
        .call::<SnapshotList>(SnapshotListArgs {})
        .await
        .unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].name, "work");
    assert!(!listed[0].in_use);
    assert!(server.home.join("snapshot/work.ext4").is_file());

    let remove = || {
        server.call::<SnapshotRemove>(SnapshotRemoveArgs {
            name: "work".into(),
        })
    };
    remove().await.unwrap();
    let listed = server
        .call::<SnapshotList>(SnapshotListArgs {})
        .await
        .unwrap();
    assert_eq!(listed, []);
    assert_eq!(code(remove().await), NOT_FOUND);
}

#[tokio::test]
async fn what_cannot_be_a_snapshot_is_refused() {
    let server = Server::spawn("snapshot-refused");
    let create = |name: Option<&str>, disk_gib| {
        server.call::<SnapshotCreate>(SnapshotCreateArgs {
            name: name.map(str::to_owned),
            disk_gib,
        })
    };
    create(Some("taken"), Some(1)).await.unwrap();

    assert_eq!(code(create(Some("taken"), Some(1)).await), ALREADY_EXISTS);
    assert_eq!(code(create(Some("zero"), Some(0)).await), INVALID_PARAMS);
    for name in ["", "../out", "a/b", ".hidden", "-flag", "spa ce"] {
        assert_eq!(
            code(create(Some(name), Some(1)).await),
            INVALID_PARAMS,
            "{name:?}"
        );
    }

    // One with no name is given one.
    let picked = create(None, None).await.unwrap().name;
    let listed = server
        .call::<SnapshotList>(SnapshotListArgs {})
        .await
        .unwrap();
    assert!(listed.iter().any(|item| item.name == picked), "{listed:?}");
}

#[tokio::test]
async fn a_snapshot_in_use_is_listed_so_and_not_removed() {
    let server = Server::spawn("snapshot-held");
    let remove = || {
        server.call::<SnapshotRemove>(SnapshotRemoveArgs {
            name: "held".into(),
        })
    };
    server
        .call::<SnapshotCreate>(SnapshotCreateArgs {
            name: Some("held".into()),
            disk_gib: Some(1),
        })
        .await
        .unwrap();

    // Locked by this process, as a VM over it would: the lock is the file beside the snapshot.
    let lock = std::fs::File::create(server.home.join("snapshot/held.ext4.lock")).unwrap();
    lock.try_lock().unwrap();
    let listed = server
        .call::<SnapshotList>(SnapshotListArgs {})
        .await
        .unwrap();
    assert!(listed[0].in_use);
    assert_eq!(code(remove().await), IN_USE);

    drop(lock);
    remove().await.unwrap();
    assert!(!server.home.join("snapshot/held.ext4.lock").exists());
}

#[tokio::test]
async fn creates_lists_and_removes_a_volume() {
    let server = Server::spawn("volume");
    let create = |name: Option<&str>| {
        server.call::<VolumeCreate>(VolumeCreateArgs {
            name: name.map(str::to_owned),
        })
    };
    assert_eq!(create(Some("data")).await.unwrap().name, "data");
    assert_eq!(code(create(Some("data")).await), ALREADY_EXISTS);
    assert_eq!(code(create(Some("a/b")).await), INVALID_PARAMS);
    let picked = create(None).await.unwrap().name;

    let mut listed: Vec<_> = server
        .call::<VolumeList>(VolumeListArgs {})
        .await
        .unwrap()
        .into_iter()
        .map(|item| item.name)
        .collect();
    listed.sort();
    let mut made = vec!["data".to_string(), picked];
    made.sort();
    assert_eq!(listed, made);

    // Held by this process, as a VM that mounts it would.
    let lock_path = server.home.join("volume/data/lock");
    let lock = std::fs::File::open(&lock_path)
        .or_else(|_| std::fs::File::create(&lock_path))
        .unwrap();
    lock.lock_shared().unwrap();
    let remove = || {
        server.call::<VolumeRemove>(VolumeRemoveArgs {
            name: "data".into(),
        })
    };
    assert_eq!(code(remove().await), IN_USE);
    drop(lock);
    remove().await.unwrap();
    assert_eq!(code(remove().await), NOT_FOUND);
}

#[tokio::test]
async fn a_mount_is_kept_at_a_path_until_it_is_removed() {
    let server = Server::spawn("mount");
    server
        .call::<VolumeCreate>(VolumeCreateArgs {
            name: Some("v".into()),
        })
        .await
        .unwrap();
    let mount = |path: &str| {
        server.call::<VolumeMountCreate>(VolumeMountCreateArgs {
            volume: "v".into(),
            path: path.into(),
            source: MountSource::Notion {
                api_key: "secret".into(),
            },
        })
    };
    let unmount = |path: &str| {
        server.call::<VolumeMountRemove>(VolumeMountRemoveArgs {
            volume: "v".into(),
            path: path.into(),
        })
    };
    let mounts = async || {
        let listed = server
            .call::<VolumeMountList>(VolumeMountListArgs { volume: "v".into() })
            .await
            .unwrap();
        listed
            .into_iter()
            .map(|m| (m.path, m.mount_type))
            .collect::<Vec<_>>()
    };

    mount("notes").await.unwrap();
    assert_eq!(mounts().await, [("notes".to_owned(), MountType::Notion)]);
    assert_eq!(code(mount("notes").await), INVALID_PARAMS);
    assert_eq!(code(mount("").await), INVALID_PARAMS);
    assert_eq!(code(mount("no/parent").await), NOT_FOUND);

    unmount("notes").await.unwrap();
    assert_eq!(mounts().await, []);
    assert_eq!(code(unmount("notes").await), NOT_FOUND);
    let answered = server
        .call::<VolumeMountList>(VolumeMountListArgs {
            volume: "nobody".into(),
        })
        .await;
    assert_eq!(code(answered), NOT_FOUND);
}

#[tokio::test]
async fn the_files_of_a_volume_are_written_listed_moved_and_read_back() {
    let server = Server::spawn("volume-files");
    server
        .call::<VolumeCreate>(VolumeCreateArgs {
            name: Some("data".into()),
        })
        .await
        .unwrap();
    let client = &server.client;
    let volume = FileRoot::Volume("data".into());

    client.create_dir(&volume, "dir").await.unwrap();
    client
        .write(&volume, "dir/a.txt", "hello world", None)
        .await
        .unwrap();
    assert!(server.home.join("volume/data/files/dir/a.txt").is_file());

    let metadata = client.stat(&volume, "dir/a.txt").await.unwrap();
    assert!(!metadata.is_dir && metadata.size == 11, "{metadata:?}");
    assert!(metadata.modified.is_some());
    let entries = client.read_dir(&volume, "dir").await.unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, "a.txt");
    assert_eq!(entries[0].metadata, metadata);
    assert_eq!(names(client.read_dir(&volume, "").await.unwrap()), ["dir"]);

    let read = client.read(&volume, "dir/a.txt", 0, None).await.unwrap();
    assert_eq!(&read[..], b"hello world");
    let read = client.read(&volume, "dir/a.txt", 6, Some(3)).await.unwrap();
    assert_eq!(&read[..], b"wor");

    client
        .rename(&volume, "dir/a.txt", "b.txt", false)
        .await
        .unwrap();
    client.copy(&volume, "b.txt", "c.txt", false).await.unwrap();
    assert_eq!(
        status(client.copy(&volume, "b.txt", "c.txt", false).await),
        412
    );
    client.copy(&volume, "b.txt", "c.txt", true).await.unwrap();
    client.copy(&volume, "dir", "copied", false).await.unwrap();
    client.remove(&volume, "dir", false).await.unwrap();
    assert_eq!(status(client.stat(&volume, "dir").await), 404);
    assert!(client.stat(&volume, "copied").await.unwrap().is_dir);
    let read = client.read(&volume, "c.txt", 0, None).await.unwrap();
    assert_eq!(&read[..], b"hello world");

    // What RFC 4918 refuses, with its status.
    assert_eq!(
        status(client.write(&volume, "no/file", "x", None).await),
        409
    );
    assert_eq!(status(client.create_dir(&volume, "no/dir").await), 409);
    assert_eq!(status(client.create_dir(&volume, "copied").await), 405);
    assert_eq!(status(client.read(&volume, "missing", 0, None).await), 404);
    assert_eq!(status(client.remove(&volume, "missing", false).await), 404);
    let nobody = FileRoot::Volume("nobody".into());
    assert_eq!(status(client.stat(&nobody, "").await), 404);

    // A link in the volume, which a machine may have made, points at nothing here.
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("/", server.home.join("volume/data/files/out")).unwrap();
        assert_eq!(status(client.stat(&volume, "out").await), 404);
        assert_eq!(status(client.read_dir(&volume, "out").await), 404);
        assert!(!names(client.read_dir(&volume, "").await.unwrap()).contains(&"out".to_owned()));
    }
}

#[tokio::test]
async fn the_files_of_a_snapshot_are_kept_in_its_image() {
    let server = Server::spawn("snapshot-files");
    server
        .call::<SnapshotCreate>(SnapshotCreateArgs {
            name: Some("work".into()),
            disk_gib: Some(1),
        })
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
        .write(&snapshot, "etc/hostname", "guest\n", None)
        .await
        .unwrap();
    assert_eq!(
        status(client.write(&snapshot, "no/file", "x", None).await),
        409
    );
    assert_eq!(status(client.create_dir(&snapshot, "etc").await), 405);
    let read = client
        .read(&snapshot, "etc/hostname", 0, None)
        .await
        .unwrap();
    assert_eq!(&read[..], b"guest\n");
    let read = client
        .read(&snapshot, "etc/hostname", 1, Some(2))
        .await
        .unwrap();
    assert_eq!(&read[..], b"ue");
    let metadata = client.stat(&snapshot, "etc/hostname").await.unwrap();
    assert_eq!((metadata.is_dir, metadata.size), (false, 6));
    assert!(metadata.modified.is_some());
    assert_eq!(
        names(client.read_dir(&snapshot, "etc").await.unwrap()),
        ["hostname"]
    );

    client
        .rename(&snapshot, "etc/hostname", "hostname", false)
        .await
        .unwrap();
    client
        .copy(&snapshot, "hostname", "etc/copy", false)
        .await
        .unwrap();
    assert_eq!(
        status(client.copy(&snapshot, "hostname", "etc/copy", false).await),
        412
    );
    client.remove(&snapshot, "hostname", false).await.unwrap();
    assert_eq!(status(client.stat(&snapshot, "hostname").await), 404);

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
        .map(|path| client.write(&snapshot, path, "x", None));
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
    assert_eq!(status(client.write(&snapshot, "x", "y", None).await), 423);
    drop(held);
    client.remove(&snapshot, "etc", false).await.unwrap();
    assert_eq!(
        names(client.read_dir(&snapshot, "").await.unwrap()),
        [] as [&str; 0]
    );
    let nobody = FileRoot::Snapshot("nobody".into());
    assert_eq!(status(client.read_dir(&nobody, "").await), 404);
}

/// What a port's URL names, as `host:port`.
fn address(url: &str) -> &str {
    url.strip_prefix("http://")
        .unwrap_or_else(|| panic!("{url} is a loopback URL"))
}

#[tokio::test]
async fn a_port_is_on_this_host_s_loopback_until_the_sandbox_goes() {
    let server = Server::spawn("ports");
    server.file_image("here:latest");
    let mut args = SandboxInitArgs::new(ImageName::parse("here:latest"));
    args.ports = vec![3000, 5900];
    let init = server.call::<SandboxInit>(args).await.unwrap();
    assert_eq!(
        init.ports
            .iter()
            .map(|opened| opened.port)
            .collect::<Vec<_>>(),
        [3000, 5900]
    );
    let listed = server
        .call::<SandboxList>(SandboxListArgs {})
        .await
        .unwrap();
    assert_eq!(listed[0].ports, init.ports);

    // Taken, and closed at once: no machine runs to reach the port in.
    let at = address(&init.ports[0].url);
    assert!(at.starts_with("127.0.0.1:"), "{at}");
    let mut stream = TcpStream::connect(at).await.unwrap();
    let mut read = Vec::new();
    assert_eq!(stream.read_to_end(&mut read).await.unwrap(), 0);

    server
        .call::<SandboxClose>(SandboxCloseArgs { sandbox: init.id })
        .await
        .unwrap();
    // Its listener goes with it, a moment after.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(TcpStream::connect(at).await.is_err());
}

/// Refused before any machine is booted.
#[tokio::test]
async fn a_sandbox_that_cannot_be_is_refused_before_it_boots() {
    let server = Server::spawn("sandbox-refused");
    let digest = server.file_image("here:latest");
    let refused = async |args: SandboxInitArgs| code(server.call::<SandboxInit>(args).await);
    let here = || SandboxInitArgs::new(ImageName::parse("here:latest"));

    for ports in [vec![0], vec![8080, 8080]] {
        let mut args = here();
        args.ports = ports;
        assert_eq!(refused(args).await, INVALID_PARAMS);
    }
    let mut args = here();
    args.vcpus = Some(0);
    assert_eq!(refused(args).await, INVALID_PARAMS);
    let mut args = here();
    args.gpu_memory_mib = Some(1024);
    assert_eq!(refused(args).await, INVALID_PARAMS);

    assert_eq!(
        refused(SandboxInitArgs::new(ImageName::parse("nothing:here"))).await,
        NOT_FOUND
    );
    let absent = format!("sha256:{}", "0".repeat(64));
    assert_ne!(absent, digest);
    assert_eq!(
        refused(SandboxInitArgs::new(ImageName::parse(&absent))).await,
        NOT_FOUND
    );
    let mut args = here();
    args.snapshot = Some("missing".into());
    assert_eq!(refused(args).await, NOT_FOUND);
    let mut args = here();
    args.volumes = vec![SandboxVolume {
        volume: "missing".into(),
        path: "/data".into(),
        readonly: false,
    }];
    assert_eq!(refused(args).await, NOT_FOUND);

    let listed = server
        .call::<SandboxList>(SandboxListArgs {})
        .await
        .unwrap();
    assert_eq!(listed, []);
    let exec = |cmd: &[&str]| {
        server.call::<SandboxExec>(SandboxExecArgs {
            sandbox: "nobody".into(),
            cmd: cmd.iter().map(|arg| arg.to_string()).collect(),
            timeout_ms: None,
        })
    };
    assert_eq!(code(exec(&["true"]).await), NOT_FOUND);
    assert_eq!(code(exec(&[]).await), INVALID_PARAMS);
    let answered = server
        .call::<SandboxClose>(SandboxCloseArgs {
            sandbox: "nobody".into(),
        })
        .await;
    assert_eq!(code(answered), NOT_FOUND);
}

/// Boots VMs: needs the network to pull alpine, the binary signed for the hypervisor, and the
/// guest kernel where libkrun finds it.
#[tokio::test]
#[ignore]
async fn a_sandbox_runs_commands_on_its_snapshot_and_volumes() {
    let server = Server::spawn("sandbox");
    server.pull_image("alpine:3.20").await;
    server
        .call::<SnapshotCreate>(SnapshotCreateArgs {
            name: Some("work".into()),
            disk_gib: Some(1),
        })
        .await
        .unwrap();
    server
        .call::<VolumeCreate>(VolumeCreateArgs {
            name: Some("data".into()),
        })
        .await
        .unwrap();
    let init = || {
        let mut args = SandboxInitArgs::new(ImageName::parse("alpine:3.20"));
        args.snapshot = Some("work".into());
        args.volumes = vec![SandboxVolume {
            volume: "data".into(),
            path: "/data".into(),
            readonly: false,
        }];
        server.call::<SandboxInit>(args)
    };
    let close = |id: String| server.call::<SandboxClose>(SandboxCloseArgs { sandbox: id });

    let id = init().await.unwrap().id;
    // One sandbox at a time on a snapshot, and its files are its alone meanwhile.
    assert_eq!(code(init().await), IN_USE);
    let snapshot = FileRoot::Snapshot("work".into());
    assert_eq!(status(server.client.read_dir(&snapshot, "").await), 423);
    assert_eq!(
        status(server.client.write(&snapshot, "x", "y", None).await),
        423
    );
    let listed = server
        .call::<SandboxList>(SandboxListArgs {})
        .await
        .unwrap();
    assert_eq!(listed.len(), 1);
    // A machine is booted for each command, and none runs yet.
    assert!(!listed[0].running);
    assert_eq!(listed[0].snapshot.as_deref(), Some("work"));

    let (mut lines, ended) = server
        .exec(&id, &["sh", "-c", "echo one; echo two >&2; exit 3"], None)
        .await;
    // stdout and stderr are read apart, so one's lines may come before the other's.
    lines.sort();
    assert_eq!(
        (lines, ended.unwrap()),
        (vec!["one".into(), "two".into()], 3)
    );
    // The guest's init could not run it, as a shell answers one it cannot find.
    let (_, ended) = server.exec(&id, &["no-such-program"], None).await;
    assert_eq!(ended.unwrap(), 127);
    let (_, ended) = server.exec(&id, &["sleep", "10"], Some(200)).await;
    assert_eq!(code(ended), TIMED_OUT);

    // A call given up on takes its machine with it, and lets go of the snapshot for the next.
    {
        let args = SandboxExecArgs {
            sandbox: id.clone(),
            cmd: ["sh", "-c", "echo started; sleep 30"]
                .map(str::to_owned)
                .to_vec(),
            timeout_ms: None,
        };
        let mut events = pin!(
            server
                .client
                .request_with_progress::<virtx_core::SandboxExecOutput, String>(
                    SandboxExec::NAME,
                    args
                )
        );
        match events.next().await {
            Some(Ok(Event::Progress(progress))) => {
                assert_eq!(progress.content.as_deref(), Some("started"));
            }
            other => panic!("expected the command's first line, got {other:?}"),
        }
    }
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let (lines, _) = server
        .exec(&id, &["sh", "-c", "ps | grep -c '[s]leep 30'"], None)
        .await;
    assert_eq!(lines, ["0"]);

    let (_, ended) = server
        .exec(
            &id,
            &[
                "sh",
                "-c",
                "echo kept > /root/kept && echo shared > /data/shared",
            ],
            None,
        )
        .await;
    assert_eq!(ended.unwrap(), 0);
    assert_eq!(
        std::fs::read_to_string(server.home.join("volume/data/files/shared")).unwrap(),
        "shared\n"
    );
    close(id.clone()).await.unwrap();
    assert_eq!(code(close(id).await), NOT_FOUND);

    // What it wrote to its snapshot reads back over the files endpoint, and what is put there
    // is in the snapshot for the next one, beside it.
    let read = server
        .client
        .read(&snapshot, "root/kept", 0, None)
        .await
        .unwrap();
    assert_eq!(&read[..], b"kept\n");
    server
        .client
        .write(&snapshot, "root/put", "put\n", None)
        .await
        .unwrap();
    let id = init().await.unwrap().id;
    let (lines, ended) = server
        .exec(&id, &["cat", "/root/kept", "/root/put"], None)
        .await;
    assert_eq!(
        (lines, ended.unwrap()),
        (vec!["kept".into(), "put".into()], 0)
    );
    close(id).await.unwrap();
    server
        .call::<SnapshotRemove>(SnapshotRemoveArgs {
            name: "work".into(),
        })
        .await
        .unwrap();
}

/// Boots VMs, as the one above does.
#[tokio::test]
#[ignore]
async fn a_port_reaches_what_listens_on_it_in_the_running_machine() {
    let server = Server::spawn("port-reached");
    server.pull_image("alpine:3.20").await;
    let mut args = SandboxInitArgs::new(ImageName::parse("alpine:3.20"));
    args.ports = vec![8000];
    let init = server.call::<SandboxInit>(args).await.unwrap();
    let at = address(&init.ports[0].url).to_owned();

    let serving = server.exec(
        &init.id,
        &[
            "sh",
            "-c",
            "while true; do echo hello | nc -l -p 8000; done",
        ],
        Some(30_000),
    );
    let reached = async {
        // Until the machine is up and something listens in it.
        loop {
            if let Ok(mut stream) = TcpStream::connect(&at).await {
                let mut said = Vec::new();
                let mut chunk = [0u8; 64];
                while let Ok(n @ 1..) = stream.read(&mut chunk).await {
                    said.extend_from_slice(&chunk[..n]);
                    if said.ends_with(b"hello\n") {
                        return said;
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    };
    tokio::select! {
        said = reached => assert_eq!(said, b"hello\n"),
        (_, ended) = serving => panic!("nc ended before it was reached: {ended:?}"),
    }
}

/// Boots VMs, as the one above does.
#[tokio::test]
#[ignore]
async fn a_snapshot_a_killed_machine_left_reads_as_its_kernel_committed() {
    let server = Server::spawn("killed");
    server.pull_image("alpine:3.20").await;
    server
        .call::<SnapshotCreate>(SnapshotCreateArgs {
            name: Some("work".into()),
            disk_gib: Some(1),
        })
        .await
        .unwrap();
    let mut args = SandboxInitArgs::new(ImageName::parse("alpine:3.20"));
    args.snapshot = Some("work".into());
    let id = server.call::<SandboxInit>(args).await.unwrap().id;

    // Synced, so committed, and then killed with the snapshot still mounted.
    let (lines, ended) = server
        .exec(
            &id,
            &[
                "sh",
                "-c",
                "mkdir /root/kept && for i in $(seq 100); do echo $i > /root/kept/$i; done \
                 && sync && echo synced && sleep 30",
            ],
            Some(5000),
        )
        .await;
    assert_eq!(lines, ["synced"]);
    assert_eq!(code(ended), TIMED_OUT);

    let path = server.home.join("snapshot/work.ext4");
    let image = std::fs::read(&path).unwrap();
    let incompat = u32::from_le_bytes(image[1024 + 0x60..1024 + 0x64].try_into().unwrap());
    assert_ne!(incompat & 0x4, 0, "left needing its journal replayed");
    let tree = FsTree::from_ext4(std::fs::File::open(&path).unwrap()).unwrap();
    let kept = tree
        .walk()
        .into_iter()
        .filter(|(path, ..)| path.starts_with(b"upper/root/kept/"))
        .count();
    assert_eq!(kept, 100);

    // And the next machine over it finds the same.
    let (lines, ended) = server.exec(&id, &["cat", "/root/kept/100"], None).await;
    assert_eq!((lines, ended.unwrap()), (vec!["100".into()], 0));
}

/// Boots VMs, as the one above does.
#[tokio::test]
#[ignore]
async fn a_built_image_runs_with_what_its_steps_made() {
    let server = Server::spawn("build");
    let recipe = Recipe::new("alpine:3.20")
        .step("apk add --no-cache jq")
        .step(Step::env("GREETING", "hello"))
        .step(Step::workdir("/work"))
        .step("echo \"$GREETING\" > note && rm /etc/motd");
    let (lines, built) = server.build(recipe, "built:latest").await;
    let built = built.unwrap();
    assert!(
        lines.contains(&"pulling docker.io/library/alpine:3.20".to_string()),
        "{lines:?}"
    );
    assert!(lines.contains(&"STEP 4/4: RUN echo \"$GREETING\" > note && rm /etc/motd".to_string()));

    let id = server
        .call::<SandboxInit>(SandboxInitArgs::new(ImageName::parse("built:latest")))
        .await
        .unwrap()
        .id;
    let (lines, ended) = server
        .exec(
            &id,
            &[
                "sh",
                "-c",
                "jq -n 1 && echo $GREETING && pwd && cat note && test ! -e /etc/motd",
            ],
            None,
        )
        .await;
    assert_eq!(
        (lines, ended.unwrap()),
        (
            ["1", "hello", "/work", "hello"].map(String::from).to_vec(),
            0
        )
    );
    let by_digest = server
        .call::<SandboxInit>(SandboxInitArgs::new(ImageName::Digest {
            digest: built.digest,
        }))
        .await;
    assert!(by_digest.is_ok());

    // A step that fails fails the build, and names nothing.
    let (_, failed) = server
        .build(Recipe::new("built:latest").step("exit 3"), "failed:latest")
        .await;
    assert_eq!(code(failed), BUILD_FAILED);
    let init = server
        .call::<SandboxInit>(SandboxInitArgs::new(ImageName::parse("failed:latest")))
        .await;
    assert_eq!(code(init), NOT_FOUND);
}

/// The images are an OCI registry at `/images/v2/` on the same stdio, beside `/rpc`.
#[tokio::test]
async fn the_images_are_a_registry_beside_the_rpc() {
    use http_body_util::{BodyExt as _, Empty};

    let home = std::env::temp_dir().join(format!("virtx-stdio-registry-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    let store = ImageStore::new(home.join("images")).unwrap();
    let layer = store.insert_layer(&FsTree::new()).unwrap();
    let image = store
        .insert_image(&Default::default(), std::slice::from_ref(&layer))
        .unwrap();
    store
        .insert_tag(&"alpine:3.21".parse().unwrap(), &image)
        .unwrap();

    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_virtx-uvm"))
        .env("VIRTX_HOME", &home)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let io = tokio::io::join(child.stdout.take().unwrap(), child.stdin.take().unwrap());
    let (mut sender, connection) = hyper::client::conn::http2::handshake(
        hyper_util::rt::TokioExecutor::new(),
        hyper_util::rt::TokioIo::new(io),
    )
    .await
    .unwrap();
    tokio::spawn(connection);
    let mut get = async |path: &str| {
        let request = http::Request::get(format!("http://virtx{path}"))
            .body(Empty::<bytes::Bytes>::new())
            .unwrap();
        let response = sender.send_request(request).await.unwrap();
        let status = response.status();
        (
            status,
            response.into_body().collect().await.unwrap().to_bytes(),
        )
    };

    let (status, body) = get("/images/v2/alpine/manifests/3.21").await;
    assert_eq!(status, http::StatusCode::OK);
    let manifest: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(manifest["layers"][0]["digest"], layer.to_string());
    assert_eq!(
        manifest["layers"][0]["mediaType"],
        "application/vnd.erofs.layer.v1"
    );
    let (status, _) = get(&format!("/images/v2/alpine/blobs/{layer}")).await;
    assert_eq!(status, http::StatusCode::OK);
    let (status, _) = get("/elsewhere").await;
    assert_eq!(status, http::StatusCode::NOT_FOUND);

    drop(sender);
    let _ = std::fs::remove_dir_all(&home);
}

/// `image.prune` takes an image no tag names once it is old, and what only it held, and leaves
/// a tagged one.
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

    let pruned = server
        .call::<ImagePrune>(ImagePruneArgs::default())
        .await
        .unwrap();
    assert_eq!(pruned.images, vec![loose.to_string()]);
    assert!(pruned.reclaimed > 0);
    assert!(!store.contains_layer(&layer));
    assert!(store.get_image(&tagged.parse().unwrap()).unwrap().is_some());
}
