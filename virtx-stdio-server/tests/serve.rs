//! The built binary, called as a client calls it: spawned, and spoken to on its stdin and
//! stdout, over a `VIRTX_HOME` of its own.

use std::path::PathBuf;
use std::pin::pin;

use futures_util::StreamExt as _;
use tokio::io::AsyncReadExt as _;
use tokio::net::TcpStream;
use tokio::process::Command;
use virtx_core::{
    Error, Event, ImageBuild, ImageBuildArgs, ImageBuildOutput, ImageName, MountSource, MountType,
    Recipe, Request, SandboxClose, SandboxCloseArgs, SandboxExec, SandboxExecArgs, SandboxInit,
    SandboxInitArgs, SandboxList, SandboxListArgs, SandboxVolume, SnapshotCreate,
    SnapshotCreateArgs, SnapshotList, SnapshotListArgs, SnapshotRemove, SnapshotRemoveArgs, Step,
    TransportClient, VolumeCreate, VolumeCreateArgs, VolumeList, VolumeListArgs, VolumeMountCreate,
    VolumeMountCreateArgs, VolumeMountList, VolumeMountListArgs, VolumeMountRemove,
    VolumeMountRemoveArgs, VolumeRemove, VolumeRemoveArgs,
};
use virtx_uvm::{FsTree, ImageStore, Registry};

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
        let registry = Registry::default();
        let (_, manifest) = registry.pull_manifest(&reference).await.unwrap();
        let config = registry.pull_config(&reference, &manifest).await.unwrap();
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
    // One sandbox at a time on a snapshot.
    assert_eq!(code(init().await), IN_USE);
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

    // What it wrote to its snapshot is there for the next one.
    let id = init().await.unwrap().id;
    let (lines, ended) = server.exec(&id, &["cat", "/root/kept"], None).await;
    assert_eq!((lines, ended.unwrap()), (vec!["kept".into()], 0));
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
