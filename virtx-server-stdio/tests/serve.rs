//! The built binary, called as a client calls it: spawned, and spoken to on its stdin and
//! stdout, over a `VIRTX_HOME` of its own -- the endpoints, and the files of its volumes and
//! sandboxes among them.

use std::path::PathBuf;

use futures_util::StreamExt as _;
use tokio::io::AsyncReadExt as _;
use tokio::net::TcpStream;
use tokio::process::Command;
use virtx_core::{
    Auth, Client, DirEntry, Error, FileRoot, ImageJobEnd, ImageJobEntry, ImageName, MountSource,
    MountType, PortProtocol, Recipe, SandboxCreateArgs, SandboxExecArgs, SandboxExecItem,
    SandboxJobEnd, SandboxJobEntry, SandboxPortOpenArgs, SandboxState, SandboxVolume, Step,
};
use virtx_uvm::{Entry, FsTree, ImageStore, RegistryClient};

/// A server over an empty `VIRTX_HOME`, removed when this drops.
struct Server {
    client: Client,
    home: PathBuf,
    /// Whether the home is this server's to remove, rather than another's it shares.
    owns: bool,
}

impl Server {
    fn spawn(name: &str) -> Server {
        Server::spawn_with(name, &[])
    }

    /// A server, as [`spawn`](Server::spawn) makes one, with `env` set for it besides.
    fn spawn_with(name: &str, env: &[(&str, &str)]) -> Server {
        let home = std::env::temp_dir().join(format!("virtx-stdio-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_virtx-stdio"));
        command.env("VIRTX_HOME", &home).envs(env.iter().copied());
        Server {
            client: Client::spawn(command).unwrap(),
            home,
            owns: true,
        }
    }

    /// Another server over this one's home, as a second client spawns one over the same
    /// `VIRTX_HOME`.
    fn beside(&self) -> Server {
        let mut command = Command::new(env!("CARGO_BIN_EXE_virtx-stdio"));
        command.env("VIRTX_HOME", &self.home);
        Server {
            client: Client::spawn(command).unwrap(),
            home: self.home.clone(),
            owns: false,
        }
    }

    /// This server's client gone, and another server spawned over its home, as the next client
    /// spawns one.
    async fn again(mut self) -> Server {
        // Gone before the next is spawned: its sandboxes stopped, and kept.
        let nowhere = Client::connect("http://127.0.0.1:9", Auth::None, Default::default());
        let gone = std::mem::replace(&mut self.client, nowhere.unwrap());
        gone.close().await;
        let mut command = Command::new(env!("CARGO_BIN_EXE_virtx-stdio"));
        command.env("VIRTX_HOME", &self.home);
        self.client = Client::spawn(command).unwrap();
        self
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
        if self.owns {
            let _ = std::fs::remove_dir_all(&self.home);
        }
    }
}

fn status<T: std::fmt::Debug>(answered: Result<T, Error>) -> u16 {
    match answered {
        Err(Error::Status(status, _) | Error::Refusal { status, .. }) => status,
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
        args.ports = ports.into_iter().map(Into::into).collect();
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
    args.disk_gib = Some(0);
    assert_eq!(refused(args).await, 400);
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
async fn a_sandbox_runs_jobs_on_its_disk_and_volumes() {
    let server = Server::spawn("sandbox");
    server.pull_image("alpine:3.20").await;
    server.client.create_volume(Some("data")).await.unwrap();
    let mut args = SandboxCreateArgs::new("alpine:3.20");
    args.name = Some("work".into());
    args.volumes = vec![SandboxVolume {
        volume: "data".into(),
        path: "/data".into(),
        readonly: false,
    }];

    let id = server.client.create_sandbox(&args).await.unwrap().id;
    // One name to one sandbox.
    assert_eq!(status(server.client.create_sandbox(&args).await), 409);
    let listed = server.client.list_sandbox().await.unwrap();
    assert_eq!(listed.len(), 1);
    // Its machine is up from its making.
    assert_eq!(listed[0].state, SandboxState::Running);
    assert_eq!(listed[0].name.as_deref(), Some("work"));
    assert!(server.home.join(format!("snapshot/{id}.ext4")).is_file());

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

    // What is written to its disk needs no `sync`: stopping the sandbox does it.
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
    let stopped = server.client.stop_sandbox(&id).await.unwrap();
    assert_eq!(stopped.state, SandboxState::Stopped);

    // Started again, by a job: what it wrote is on its disk still.
    let (lines, ended) = server.exec(&id, &["cat", "/root/kept"], None).await;
    assert_eq!((lines, ended.unwrap()), (vec!["kept".into()], 0));
    assert_eq!(
        server
            .client
            .find_sandbox("work")
            .await
            .unwrap()
            .unwrap()
            .state,
        SandboxState::Running
    );

    // Committed: an image another sandbox begins as this one is.
    let job = server.client.commit_image(&id, "kept:v1").await.unwrap();
    let ended: Vec<_> = server.client.image_job_log(&job.id, None).collect().await;
    assert!(
        server
            .client
            .get_image_job(&job.id)
            .await
            .unwrap()
            .digest
            .is_some(),
        "{ended:?}"
    );
    let other = server
        .client
        .create_sandbox(&SandboxCreateArgs::new("kept:v1"))
        .await
        .unwrap()
        .id;
    let (lines, ended) = server.exec(&other, &["cat", "/root/kept"], None).await;
    assert_eq!((lines, ended.unwrap()), (vec!["kept".into()], 0));

    server.client.remove_sandbox(&id).await.unwrap();
    assert_eq!(status(server.client.remove_sandbox(&id).await), 404);
    assert!(!server.home.join(format!("snapshot/{id}.ext4")).exists());
    server.client.remove_sandbox(&other).await.unwrap();
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

    // A link in the volume, which a machine may have made, points at nothing here.
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("/", server.home.join("volume/data/files/out")).unwrap();
        assert_eq!(status(client.stat_file(&volume, "out").await), 404);
        assert_eq!(status(client.read_dir(&volume, "out").await), 404);
        assert!(!names(client.read_dir(&volume, "").await.unwrap()).contains(&"out".to_owned()));
    }
}

/// Boots VMs, as the one above does.
#[tokio::test]
#[ignore]
async fn a_sandbox_is_kept_across_a_server_started_again() {
    let server = Server::spawn("kept");
    server.pull_image("alpine:3.20").await;
    let mut args = SandboxCreateArgs::new("alpine:3.20");
    args.name = Some("kept".into());
    args.ports = vec![8000.into()];
    let id = server.client.create_sandbox(&args).await.unwrap().id;
    let (_, ended) = server
        .exec(&id, &["sh", "-c", "echo kept > /root/kept"], None)
        .await;
    assert_eq!(ended.unwrap(), 0);
    let ephemeral = server
        .client
        .create_sandbox(&SandboxCreateArgs::new("alpine:3.20").ephemeral(true))
        .await
        .unwrap()
        .id;

    // Its client gone, the server stops it, and the next one over the home knows it stopped.
    let server = server.again().await;
    let found = server.client.find_sandbox("kept").await.unwrap().unwrap();
    assert_eq!(
        (found.id.as_str(), found.state),
        (id.as_str(), SandboxState::Stopped)
    );
    assert_eq!(status(server.client.get_sandbox(&ephemeral).await), 404);
    assert!(
        !server
            .home
            .join(format!("snapshot/{ephemeral}.ext4"))
            .exists()
    );

    // Started again, as it was left, its port answered where this server answers it.
    let started = server.client.start_sandbox(&id).await.unwrap();
    assert_eq!(started.state, SandboxState::Running);
    assert_eq!(started.ports.len(), 1);
    assert!(started.ports[0].url.starts_with("http://127.0.0.1:"));
    let (lines, ended) = server.exec(&id, &["cat", "/root/kept"], None).await;
    assert_eq!((lines, ended.unwrap()), (vec!["kept".into()], 0));
    server.client.remove_sandbox(&id).await.unwrap();
}

/// Boots VMs, as the one above does.
#[tokio::test]
#[ignore]
async fn a_sandbox_running_in_another_server_is_refused() {
    let server = Server::spawn("beside");
    server.pull_image("alpine:3.20").await;
    let id = server
        .client
        .create_sandbox(&SandboxCreateArgs::new("alpine:3.20"))
        .await
        .unwrap()
        .id;
    let (_, ended) = server
        .exec(&id, &["sh", "-c", "echo kept > /root/kept"], None)
        .await;
    assert_eq!(ended.unwrap(), 0);

    // Its disk is held by the machine the first runs: the second starts it not, nor deletes
    // it, nor reads its files.
    let other = server.beside();
    assert_eq!(
        other.client.get_sandbox(&id).await.unwrap().state,
        SandboxState::Stopped
    );
    assert_eq!(status(other.client.start_sandbox(&id).await), 409);
    assert_eq!(status(other.client.remove_sandbox(&id).await), 409);
    let root = FileRoot::Sandbox(id.clone());
    assert_eq!(
        status(other.client.read_text(&root, "root/kept").await),
        409
    );
    let (lines, ended) = server.exec(&id, &["cat", "/root/kept"], None).await;
    assert_eq!((lines, ended.unwrap()), (vec!["kept".into()], 0));

    // Stopped there, it is the other's to start.
    server.client.stop_sandbox(&id).await.unwrap();
    let (lines, ended) = other.exec(&id, &["cat", "/root/kept"], None).await;
    assert_eq!((lines, ended.unwrap()), (vec!["kept".into()], 0));
    other.client.remove_sandbox(&id).await.unwrap();
}

/// Boots VMs, as the one above does.
#[tokio::test]
#[ignore]
async fn an_unused_sandbox_stops_and_starts_again_with_what_it_wrote() {
    let server = Server::spawn_with("auto-stop", &[("VIRTX_MINUTE_MS", "500")]);
    server.pull_image("alpine:3.20").await;
    let args = SandboxCreateArgs::new("alpine:3.20").auto_stop_minutes(1);
    let id = server.client.create_sandbox(&args).await.unwrap().id;
    let state = async || server.client.get_sandbox(&id).await.unwrap().state;
    let root = FileRoot::Sandbox(id.clone());
    // What runs past its job ends with the machine, so seeing it gone is seeing it stopped.
    let behind = ["sh", "-c", "sleep 1000 >/dev/null 2>&1 &"];

    let (_, ended) = server
        .exec(&id, &["sh", "-c", "echo kept > /root/kept"], None)
        .await;
    assert_eq!(ended.unwrap(), 0);
    let (_, ended) = server.exec(&id, &behind, None).await;
    assert_eq!(ended.unwrap(), 0);
    // A job that runs keeps it up, however long.
    let (lines, ended) = server
        .exec(
            &id,
            &["sh", "-c", "sleep 3; ps | grep -c '[s]leep 1000'"],
            None,
        )
        .await;
    assert_eq!((lines, ended.unwrap()), (vec!["1".into()], 0));

    // Unused, it stops, and says so; its files are not read then, nor is it started for them.
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    assert_eq!(state().await, SandboxState::Stopped);
    assert_eq!(
        status(server.client.read_text(&root, "root/kept").await),
        409
    );
    assert_eq!(state().await, SandboxState::Stopped);

    // A job starts it again, over what it wrote.
    let (lines, _) = server
        .exec(
            &id,
            &["sh", "-c", "cat /root/kept; ps | grep -c '[s]leep 1000'"],
            None,
        )
        .await;
    assert_eq!(lines, ["kept", "0"]);
    assert_eq!(state().await, SandboxState::Running);

    // A request to its files is a use of it, and what it wrote is there after it stops again.
    server
        .client
        .write_file(&root, "root/written", "written\n", None)
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    assert_eq!(state().await, SandboxState::Stopped);
    let (lines, _) = server.exec(&id, &["cat", "/root/written"], None).await;
    assert_eq!(lines, ["written"]);

    server.client.remove_sandbox(&id).await.unwrap();
    let left = std::fs::read_dir(server.home.join("sandbox"))
        .unwrap()
        .count();
    assert_eq!(left, 0);
}

/// Boots VMs, as the one above does.
#[tokio::test]
#[ignore]
async fn a_sandbox_stopped_while_it_stops_unused_answers_once_it_has_synced() {
    let server = Server::spawn_with("auto-stop-stop", &[("VIRTX_MINUTE_MS", "500")]);
    server.pull_image("alpine:3.20").await;
    // It stops some 500 to 1000 ms after its last job ends, and syncs for a while: one stop
    // asked for per delay across that.
    for delay in (400..=1100).step_by(100) {
        let args = SandboxCreateArgs::new("alpine:3.20").auto_stop_minutes(1);
        let id = server.client.create_sandbox(&args).await.unwrap().id;
        // Left in the guest's page cache, so stopping the machine takes the while it syncs.
        let (_, ended) = server
            .exec(
                &id,
                &["dd", "if=/dev/zero", "of=/root/big", "bs=1M", "count=64"],
                None,
            )
            .await;
        assert_eq!(ended.unwrap(), 0);

        // Asked to stop as it may be stopping already, it answers once its machine has synced,
        // so its disk holds all it wrote.
        tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
        server.client.stop_sandbox(&id).await.unwrap();
        let disk = std::fs::File::open(server.home.join(format!("snapshot/{id}.ext4"))).unwrap();
        let tree = FsTree::from_ext4(disk).unwrap();
        let size = match tree.entry(b"upper/root/big").unwrap() {
            Some(Entry::File { data, .. }) => data.len(),
            other => panic!("{other:?}, stopped after {delay} ms"),
        };
        assert_eq!(size, 64 << 20, "stopped after {delay} ms");
        server.client.remove_sandbox(&id).await.unwrap();
    }
}

/// Boots VMs, as the one above does.
#[tokio::test]
#[ignore]
async fn a_port_is_opened_and_closed_as_the_sandbox_runs() {
    let server = Server::spawn("port-opened");
    server.pull_image("alpine:3.20").await;
    let id = server
        .client
        .create_sandbox(&SandboxCreateArgs::new("alpine:3.20"))
        .await
        .unwrap()
        .id;
    let mut serve = SandboxExecArgs::new([
        "sh",
        "-c",
        "while true; do echo hello | nc -l -p 8000; done",
    ]);
    serve.timeout_ms = Some(30_000);
    server.client.exec_sandbox(&id, &serve).await.unwrap();

    let args = SandboxPortOpenArgs {
        label: Some("hello".into()),
        protocol: Some(PortProtocol::Http),
    };
    let open = server
        .client
        .open_sandbox_port_with(&id, 8000, &args)
        .await
        .unwrap();
    assert_eq!(open.label.as_deref(), Some("hello"));
    let at = address(&open.url).to_owned();
    assert_eq!(said(&at).await, b"hello\n");
    // Opened again: where it is, its protocol kept.
    let tcp = SandboxPortOpenArgs {
        label: None,
        protocol: Some(PortProtocol::Tcp),
    };
    let again = server.client.open_sandbox_port_with(&id, 8000, &tcp).await;
    assert_eq!(status(again), 409);
    let again = SandboxPortOpenArgs {
        label: Some("hi".into()),
        protocol: None,
    };
    let again = server
        .client
        .open_sandbox_port_with(&id, 8000, &again)
        .await
        .unwrap();
    assert_eq!(
        (again.url.as_str(), again.label.as_deref()),
        (open.url.as_str(), Some("hi"))
    );
    let started = server.client.start_sandbox(&id).await.unwrap();
    assert_eq!(started.ports, [again]);

    // Closed: reached no more, and not opened again as it starts.
    server.client.close_sandbox_port(&id, 8000).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert!(TcpStream::connect(&at).await.is_err());
    assert!(server.client.close_sandbox_port(&id, 8000).await.is_err());
    server.client.stop_sandbox(&id).await.unwrap();
    assert_eq!(server.client.start_sandbox(&id).await.unwrap().ports, []);
    server.client.remove_sandbox(&id).await.unwrap();
}

/// What a listener at `at` says, once something listens behind it.
async fn said(at: &str) -> Vec<u8> {
    let reading = async {
        loop {
            if let Ok(mut stream) = TcpStream::connect(at).await {
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
    tokio::time::timeout(std::time::Duration::from_secs(30), reading)
        .await
        .expect("nothing answered")
}

/// Boots VMs, as the one above does.
#[tokio::test]
#[ignore]
async fn a_job_s_stdin_is_written_and_its_log_answers_its_exec() {
    let server = Server::spawn("stdin");
    server.pull_image("alpine:3.20").await;
    let id = server
        .client
        .create_sandbox(&SandboxCreateArgs::new("alpine:3.20"))
        .await
        .unwrap()
        .id;
    let client = &server.client;

    let mut cat = SandboxExecArgs::new(["sh", "-c", "cat; echo done"]);
    cat.stdin = true;
    let job = client.exec_sandbox(&id, &cat).await.unwrap();
    client
        .write_sandbox_job_stdin(&id, &job.id, Some("one\ntwo\n"), false)
        .await
        .unwrap();
    client
        .write_sandbox_job_stdin(&id, &job.id, None::<String>, true)
        .await
        .unwrap();
    let mut lines = Vec::new();
    let mut log = client.sandbox_job_log(&id, &job.id, None);
    while let Some(entry) = log.next().await {
        match entry.unwrap().entry {
            SandboxJobEntry::Stdout(line) => lines.push(line),
            SandboxJobEntry::End(end) => {
                assert_eq!(end.code, 0);
                break;
            }
            _ => {}
        }
    }
    assert_eq!(lines, ["one", "two", "done"]);
    // Its stdin closed, or one never given: refused.
    let written = client
        .write_sandbox_job_stdin(&id, &job.id, Some("x"), false)
        .await;
    assert_eq!(status(written), 409);
    let plain = client
        .exec_sandbox(&id, &SandboxExecArgs::new(["sleep", "5"]))
        .await
        .unwrap();
    let written = client
        .write_sandbox_job_stdin(&id, &plain.id, Some("x"), false)
        .await;
    assert_eq!(status(written), 409);

    // Its exec answered with its log, in one request.
    let args = SandboxExecArgs::new(["sh", "-c", "echo héllo; exit 4"]);
    let items: Vec<_> = client.exec_sandbox_with_log(&id, &args).collect().await;
    let items: Vec<_> = items.into_iter().map(Result::unwrap).collect();
    let SandboxExecItem::Job(job) = &items[0] else {
        panic!("{items:?}");
    };
    assert_eq!(job.cmd, args.cmd);
    let entries: Vec<_> = items[1..]
        .iter()
        .map(|item| match item {
            SandboxExecItem::Log(entry) => entry.entry.clone(),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(
        entries,
        [
            SandboxJobEntry::Stdout("héllo".into()),
            SandboxJobEntry::End(SandboxJobEnd { code: 4 })
        ]
    );
    server.client.remove_sandbox(&id).await.unwrap();
}

#[tokio::test]
#[ignore]
async fn the_files_of_a_sandbox_are_what_its_processes_see() {
    let server = Server::spawn("sandbox-files");
    server.pull_image("alpine:3.20").await;
    server.client.create_volume(Some("data")).await.unwrap();
    let mut args = SandboxCreateArgs::new("alpine:3.20");
    args.volumes = vec![SandboxVolume {
        volume: "data".into(),
        path: "/data".into(),
        readonly: true,
    }];
    let id = server.client.create_sandbox(&args).await.unwrap().id;
    let client = &server.client;
    let root = FileRoot::Sandbox(id.clone());

    // The root as a process sees it, but for the kernel's filesystems and the agent's.
    let listed = names(client.read_dir(&root, "").await.unwrap());
    for shown in ["etc", "root", "data"] {
        assert!(listed.contains(&shown.into()), "{listed:?}");
    }
    for hidden in ["proc", "sys", "dev", ".virtx"] {
        assert!(!listed.contains(&hidden.into()), "{listed:?}");
    }
    assert_eq!(status(client.stat_file(&root, "proc/self").await), 404);

    // What is written here a process reads at once, and the other way round.
    client
        .write_file(&root, "root/put", "put\n", None)
        .await
        .unwrap();
    let (lines, ended) = server.exec(&id, &["cat", "/root/put"], None).await;
    assert_eq!((lines, ended.unwrap()), (vec!["put".into()], 0));
    let (_, ended) = server
        .exec(&id, &["sh", "-c", "echo made > /root/made"], None)
        .await;
    assert_eq!(ended.unwrap(), 0);
    assert_eq!(
        client.read_text(&root, "root/made").await.unwrap(),
        "made\n"
    );

    // A file larger than one request carries goes and comes back whole.
    let big: Vec<u8> = (0..3_500_000u32).map(|i| (i % 251) as u8).collect();
    client
        .write_file(&root, "root/big", big.clone(), None)
        .await
        .unwrap();
    let (lines, _) = server.exec(&id, &["wc", "-c", "/root/big"], None).await;
    assert_eq!(lines, ["3500000 /root/big"]);
    let read = client.read_file(&root, "root/big", 0, None).await.unwrap();
    assert!(read[..] == big[..], "{} bytes read back", read.len());

    // A file written over keeps its mode, and runs as it did.
    client
        .write_file(&root, "root/run", "#!/bin/sh\necho one\n", None)
        .await
        .unwrap();
    let (_, ended) = server.exec(&id, &["chmod", "755", "/root/run"], None).await;
    assert_eq!(ended.unwrap(), 0);
    client
        .write_file(&root, "root/run", "#!/bin/sh\necho two\n", None)
        .await
        .unwrap();
    let (lines, ended) = server.exec(&id, &["/root/run"], None).await;
    assert_eq!((lines, ended.unwrap()), (vec!["two".into()], 0));

    // Directories are made, and files moved, copied and removed in them.
    client.create_dir_all(&root, "work/a/b").await.unwrap();
    client
        .rename_file(&root, "root/put", "work/a/b/moved", false)
        .await
        .unwrap();
    client
        .copy_file(&root, "work/a/b/moved", "work/copied", false)
        .await
        .unwrap();
    assert_eq!(
        names(client.read_dir(&root, "work").await.unwrap()).len(),
        2
    );
    client.remove_file(&root, "work", false).await.unwrap();
    let (_, ended) = server.exec(&id, &["test", "-e", "/work"], None).await;
    assert_eq!(ended.unwrap(), 1);

    // A link is not shown, a read-only volume takes no write, and the root is not removed.
    let (_, ended) = server
        .exec(&id, &["ln", "-s", "/etc/hostname", "/root/link"], None)
        .await;
    assert_eq!(ended.unwrap(), 0);
    assert!(!names(client.read_dir(&root, "root").await.unwrap()).contains(&"link".into()));
    assert_eq!(
        status(client.read_file(&root, "root/link", 0, None).await),
        404
    );
    assert_eq!(
        status(client.write_file(&root, "data/x", "x", None).await),
        403
    );
    assert_eq!(status(client.remove_file(&root, "", false).await), 403);

    // A pipe is neither read nor written, and holds up nothing asked after it.
    let (_, ended) = server.exec(&id, &["mkfifo", "/root/fifo"], None).await;
    assert_eq!(ended.unwrap(), 0);
    let fifo = async {
        assert_eq!(
            status(client.copy_file(&root, "root/run", "root/fifo", true).await),
            403
        );
        assert_eq!(
            status(client.read_file(&root, "root/fifo", 0, None).await),
            404
        );
        client.read_text(&root, "root/made").await.unwrap()
    };
    let read = tokio::time::timeout(std::time::Duration::from_secs(10), fifo).await;
    assert_eq!(read.expect("a pipe held the files up"), "made\n");

    // Stopped, its files are not reached, and it is not started for them.
    server.client.stop_sandbox(&id).await.unwrap();
    assert_eq!(status(client.read_dir(&root, "").await), 409);
    assert_eq!(
        server.client.get_sandbox(&id).await.unwrap().state,
        SandboxState::Stopped
    );
    server.client.remove_sandbox(&id).await.unwrap();
    assert_eq!(status(client.read_dir(&root, "").await), 404);
}

/// What a port's URL names, as `host:port`.
fn address(url: &str) -> &str {
    url.strip_prefix("http://")
        .unwrap_or_else(|| panic!("{url} is a loopback URL"))
}

#[tokio::test]
#[ignore]
async fn a_port_reaches_what_listens_on_it_until_the_sandbox_goes() {
    let server = Server::spawn("port-reached");
    server.pull_image("alpine:3.20").await;
    let mut args = SandboxCreateArgs::new("alpine:3.20");
    args.ports = vec![8000.into()];
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
async fn a_disk_a_stopped_machine_left_reads_as_its_kernel_committed() {
    let server = Server::spawn("killed");
    server.pull_image("alpine:3.20").await;
    let args = SandboxCreateArgs::new("alpine:3.20");
    let id = server.client.create_sandbox(&args).await.unwrap().id;

    // Synced, so committed, and then stopped with its disk still mounted.
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
    server.client.stop_sandbox(&id).await.unwrap();

    let path = server.home.join(format!("snapshot/{id}.ext4"));
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

    // And its next machine finds the same.
    server.client.start_sandbox(&id).await.unwrap();
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
    // Looked for a minute: what is tried is that it goes, not how soon.
    for _ in 0..600 {
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
