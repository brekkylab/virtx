//! The built binary, called as a client calls it: spawned, and spoken to on its stdin and
//! stdout, over a `VIRTX_HOME` of its own -- the endpoints, and the files of its volumes and
//! snapshots among them.

use std::path::PathBuf;

use futures_util::StreamExt as _;
use tokio::io::AsyncReadExt as _;
use tokio::net::TcpStream;
use tokio::process::Command;
use virtx_core::{
    Client, DirEntry, Error, FileRoot, ImageJobEnd, ImageJobEntry, ImageName, MountSource,
    MountType, Recipe, SandboxCreateArgs, SandboxExecArgs, SandboxJobEntry, SandboxVolume, Step,
};
use virtx_uvm::{Entry, FsTree, ImageStore, RegistryClient};

/// A server over an empty `VIRTX_HOME`, removed when this drops.
struct Server {
    client: Client,
    home: PathBuf,
}

impl Server {
    fn spawn(name: &str) -> Server {
        let home = std::env::temp_dir().join(format!("virtx-stdio-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_virtx-stdio"));
        command.env("VIRTX_HOME", &home);
        Server {
            client: Client::spawn(command).unwrap(),
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

    /// What `cmd` wrote in `sandbox`, a line at a time, and how it ended.
    async fn exec(
        &self,
        sandbox: &str,
        cmd: &[&str],
        timeout_ms: Option<u64>,
    ) -> (Vec<String>, Result<i32, Error>) {
        let mut args = SandboxExecArgs::new(cmd.iter().copied());
        args.timeout_ms = timeout_ms;
        let job = match self.client.exec_sandbox(sandbox, &args).await {
            Ok(job) => job,
            Err(e) => return (Vec::new(), Err(e)),
        };
        let mut log = self.client.sandbox_job_log(sandbox, &job.id, None);
        let mut lines = Vec::new();
        while let Some(entry) = log.next().await {
            match entry.unwrap().entry {
                SandboxJobEntry::Stdout(line) | SandboxJobEntry::Stderr(line) => lines.push(line),
                SandboxJobEntry::End(end) => return (lines, Ok(end.code)),
                SandboxJobEntry::Truncated {} => {}
            }
        }
        unreachable!("a job's log ends with its end")
    }

    /// What building `recipe` as `reference` logged, a line at a time, and how it ended.
    async fn build(&self, recipe: Recipe, reference: &str) -> (Vec<String>, ImageJobEnd) {
        let job = self.client.build_image(recipe, reference).await.unwrap();
        let mut log = self.client.image_job_log(&job.id, None);
        let mut lines = Vec::new();
        while let Some(entry) = log.next().await {
            match entry.unwrap().entry {
                ImageJobEntry::Log(line) => lines.push(line),
                ImageJobEntry::End(end) => return (lines, end),
                ImageJobEntry::Truncated {} => {}
            }
        }
        unreachable!("a job's log ends with its end")
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

fn status<T: std::fmt::Debug>(answered: Result<T, Error>) -> u16 {
    match answered {
        Err(Error::Status(status, _)) => status,
        other => panic!("expected a refusal, got {other:?}"),
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
async fn creates_lists_and_removes_a_snapshot() {
    let server = Server::spawn("snapshot");
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
    let server = Server::spawn("snapshot-refused");
    let create =
        async |name: Option<&str>, disk_gib| server.client.create_snapshot(name, disk_gib).await;
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
    let server = Server::spawn("snapshot-held");
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
    assert_eq!(status(mount("notes").await), 400);
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
    let refused = async |args: SandboxCreateArgs| status(server.client.create_sandbox(&args).await);
    let here = || SandboxCreateArgs::new("here:latest");

    for ports in [vec![0], vec![8080, 8080]] {
        let mut args = here();
        args.ports = ports;
        assert_eq!(refused(args).await, 400);
    }
    let mut args = here();
    args.vcpus = Some(0);
    assert_eq!(refused(args).await, 400);
    let mut args = here();
    args.gpu_memory_mib = Some(1024);
    assert_eq!(refused(args).await, 400);

    assert_eq!(refused(SandboxCreateArgs::new("nothing:here")).await, 404);
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

    assert_eq!(server.client.list_sandbox().await.unwrap(), []);
    let exec = async |cmd: &[&str]| {
        let args = SandboxExecArgs::new(cmd.iter().copied());
        server.client.exec_sandbox("nobody", &args).await
    };
    assert_eq!(status(exec(&["true"]).await), 404);
    assert_eq!(status(exec(&[]).await), 400);
    assert_eq!(status(server.client.remove_sandbox("nobody").await), 404);
}

/// Boots VMs: needs the network to pull alpine, the binary signed for the hypervisor, and the
/// guest kernel where libkrun finds it.
#[tokio::test]
#[ignore]
async fn a_sandbox_runs_jobs_on_its_snapshot_and_volumes() {
    let server = Server::spawn("sandbox");
    server.pull_image("alpine:3.20").await;
    server
        .client
        .create_snapshot(Some("work"), Some(1))
        .await
        .unwrap();
    server.client.create_volume(Some("data")).await.unwrap();
    let create = async || {
        let mut args = SandboxCreateArgs::new("alpine:3.20");
        args.snapshot = Some("work".into());
        args.volumes = vec![SandboxVolume {
            volume: "data".into(),
            path: "/data".into(),
            readonly: false,
        }];
        server.client.create_sandbox(&args).await
    };

    let id = create().await.unwrap().id;
    // One sandbox at a time on a snapshot, and its files are its alone meanwhile.
    assert_eq!(status(create().await), 409);
    let snapshot = FileRoot::Snapshot("work".into());
    assert_eq!(status(server.client.read_dir(&snapshot, "").await), 423);
    assert_eq!(
        status(server.client.write_file(&snapshot, "x", "y", None).await),
        423
    );
    let listed = server.client.list_sandbox().await.unwrap();
    assert_eq!(listed.len(), 1);
    // Its machine is up from its making.
    assert!(listed[0].running);
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
    let (_, ended) = server.exec(&id, &["no-such-program"], None).await;
    assert_eq!(status(ended), 422);
    let (_, ended) = server.exec(&id, &["sleep", "10"], Some(200)).await;
    assert_eq!(ended.unwrap(), 137);
    // A line past what a frame carries, escaped six times over as JSON, comes in pieces, and
    // the machine goes on.
    let (lines, ended) = server
        .exec(
            &id,
            &["sh", "-c", "head -c 20000000 /dev/zero | tr '\\0' '\\1'"],
            None,
        )
        .await;
    assert_eq!((lines.concat().len(), ended.unwrap()), (20_000_000, 0));

    // Jobs run side by side, and one cancelled takes what it started with it.
    let job = server
        .client
        .exec_sandbox(&id, &SandboxExecArgs::new(["sleep", "30"]))
        .await
        .unwrap();
    assert!(
        server
            .client
            .get_sandbox_job(&id, &job.id)
            .await
            .unwrap()
            .running
    );
    server
        .client
        .remove_sandbox_job(&id, &job.id)
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let (lines, _) = server
        .exec(&id, &["sh", "-c", "ps | grep -c '[s]leep 30'"], None)
        .await;
    assert_eq!(lines, ["0"]);

    // What is written to the snapshot needs no `sync`: removing the sandbox does it.
    let (_, ended) = server
        .exec(
            &id,
            &[
                "sh",
                "-c",
                "echo shared > /data/shared && sync && echo kept > /root/kept",
            ],
            None,
        )
        .await;
    assert_eq!(ended.unwrap(), 0);
    assert_eq!(
        std::fs::read_to_string(server.home.join("volume/data/files/shared")).unwrap(),
        "shared\n"
    );
    server.client.remove_sandbox(&id).await.unwrap();
    assert_eq!(status(server.client.remove_sandbox(&id).await), 404);

    // What it wrote to its snapshot reads back over the files endpoint, and what is put there
    // is in the snapshot for the next one, beside it.
    let read = server
        .client
        .read_file(&snapshot, "root/kept", 0, None)
        .await
        .unwrap();
    assert_eq!(&read[..], b"kept\n");
    server
        .client
        .write_file(&snapshot, "root/put", "put\n", None)
        .await
        .unwrap();
    let id = create().await.unwrap().id;
    let (lines, ended) = server
        .exec(&id, &["cat", "/root/kept", "/root/put"], None)
        .await;
    assert_eq!(
        (lines, ended.unwrap()),
        (vec!["kept".into(), "put".into()], 0)
    );
    server.client.remove_sandbox(&id).await.unwrap();
    server.client.remove_snapshot("work").await.unwrap();
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

    // A link in the volume, which a machine may have made, points at nothing here.
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("/", server.home.join("volume/data/files/out")).unwrap();
        assert_eq!(status(client.stat_file(&volume, "out").await), 404);
        assert_eq!(status(client.read_dir(&volume, "out").await), 404);
        assert!(!names(client.read_dir(&volume, "").await.unwrap()).contains(&"out".to_owned()));
    }
}

#[tokio::test]
async fn the_files_of_a_snapshot_are_kept_in_its_image() {
    let server = Server::spawn("snapshot-files");
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

/// What a port's URL names, as `host:port`.
fn address(url: &str) -> &str {
    url.strip_prefix("http://")
        .unwrap_or_else(|| panic!("{url} is a loopback URL"))
}

/// Boots VMs, as the one above does.
#[tokio::test]
#[ignore]
async fn a_port_reaches_what_listens_on_it_until_the_sandbox_goes() {
    let server = Server::spawn("port-reached");
    server.pull_image("alpine:3.20").await;
    let mut args = SandboxCreateArgs::new("alpine:3.20");
    args.ports = vec![8000];
    let created = server.client.create_sandbox(&args).await.unwrap();
    let at = address(&created.ports[0].url).to_owned();
    assert!(at.starts_with("127.0.0.1:"), "{at}");
    // Answered only on its making: the URLs are as secret as what reaches them.
    assert_eq!(
        server.client.get_sandbox(&created.id).await.unwrap().ports,
        []
    );

    let mut serve = SandboxExecArgs::new([
        "sh",
        "-c",
        "while true; do echo hello | nc -l -p 8000; done",
    ]);
    serve.timeout_ms = Some(30_000);
    server
        .client
        .exec_sandbox(&created.id, &serve)
        .await
        .unwrap();
    let said = async {
        // Until something listens in it.
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
    assert_eq!(said.await, b"hello\n");

    server.client.remove_sandbox(&created.id).await.unwrap();
    // Its listener goes with it, a moment after.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert!(TcpStream::connect(&at).await.is_err());
}

/// Boots VMs, as the one above does.
#[tokio::test]
#[ignore]
async fn a_snapshot_a_killed_machine_left_reads_as_its_kernel_committed() {
    let server = Server::spawn("killed");
    server.pull_image("alpine:3.20").await;
    server
        .client
        .create_snapshot(Some("work"), Some(1))
        .await
        .unwrap();
    let mut args = SandboxCreateArgs::new("alpine:3.20");
    args.snapshot = Some("work".into());
    let id = server.client.create_sandbox(&args).await.unwrap().id;

    // Synced, so committed, and then killed with the snapshot still mounted.
    let (lines, ended) = server
        .exec(
            &id,
            &[
                "sh",
                "-c",
                "mkdir /root/kept && for i in $(seq 100); do echo $i > /root/kept/$i; done \
                 && sync && echo synced",
            ],
            None,
        )
        .await;
    assert_eq!((lines, ended.unwrap()), (vec!["synced".into()], 0));
    server.client.remove_sandbox(&id).await.unwrap();

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
    let id = server.client.create_sandbox(&args).await.unwrap().id;
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
    let digest = built.digest.unwrap_or_else(|| panic!("{:?}", built.error));
    assert!(
        lines.contains(&"pulling docker.io/library/alpine:3.20".to_string()),
        "{lines:?}"
    );
    assert!(lines.contains(&"STEP 4/4: RUN echo \"$GREETING\" > note && rm /etc/motd".to_string()));

    let id = server
        .client
        .create_sandbox(&SandboxCreateArgs::new("built:latest"))
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
    let by_digest = SandboxCreateArgs::new(ImageName::Digest { digest });
    assert!(server.client.create_sandbox(&by_digest).await.is_ok());

    // A step that fails fails the build, and names nothing.
    let (_, failed) = server
        .build(Recipe::new("built:latest").step("exit 3"), "failed:latest")
        .await;
    assert!(failed.error.is_some(), "{failed:?}");
    let created = server
        .client
        .create_sandbox(&SandboxCreateArgs::new("failed:latest"))
        .await;
    assert_eq!(status(created), 404);
}

/// Boots VMs, as the one above does.
#[tokio::test]
#[ignore]
async fn a_cancelled_build_takes_its_machine_with_it() {
    let server = Server::spawn("cancel");
    let recipe = Recipe::new("alpine:3.20").step("echo started; sleep infinity");
    let job = server
        .client
        .build_image(recipe, "never:latest")
        .await
        .unwrap();
    let mut log = server.client.image_job_log(&job.id, None);
    loop {
        match log.next().await.unwrap().unwrap().entry {
            ImageJobEntry::Log(line) if line == "started" => break,
            ImageJobEntry::End(end) => panic!("{end:?}"),
            _ => {}
        }
    }
    // A machine's process names the server's home on its command line, in its rootfs's path.
    let running = || {
        std::process::Command::new("pgrep")
            .arg("-f")
            .arg(&server.home)
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap()
            .success()
    };
    assert!(running());

    server.client.remove_image_job(&job.id).await.unwrap();
    for _ in 0..50 {
        if !running() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("the build's machine outlived its job");
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

/// A pull's digest is the image it tagged here, not the registry's index it chose from.
/// Needs the network to pull alpine.
#[tokio::test]
#[ignore]
async fn a_pull_answers_the_image_it_tagged() {
    let server = Server::spawn("pull");
    let job = server.client.pull_image("alpine:3.20", None).await.unwrap();
    let mut log = server.client.image_job_log(&job.id, None);
    let end = loop {
        if let ImageJobEntry::End(end) = log.next().await.unwrap().unwrap().entry {
            break end;
        }
    };
    let digest = end.digest.unwrap_or_else(|| panic!("{:?}", end.error));
    let listed = server.client.list_image().await.unwrap();
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(listed[0].digest, digest);
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
