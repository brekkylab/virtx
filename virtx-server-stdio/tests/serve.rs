//! The built binary, called as a client calls it: spawned, and spoken to on its stdin and
//! stdout, over a `VIRTX_HOME` of its own -- what its sandboxes' machines do, each test booting
//! them, and so ignored but where `--include-ignored` asks. What is answered without a machine
//! is the app's own tests, in `src/main.rs`.

use std::path::PathBuf;

use futures_util::StreamExt as _;
use tokio::io::AsyncReadExt as _;
use tokio::net::TcpStream;
use tokio::process::Command;
use virtx_core::{
    Auth, Client, DirEntry, Error, FileRoot, ImageJobEnd, ImageJobEntry, ImageName, Recipe,
    SandboxCreateArgs, SandboxExecArgs, SandboxExecItem, SandboxJobEnd, SandboxJobEntry,
    SandboxVolume, Step,
};
use virtx_uvm::{Entry, FsTree, ImageStore, RegistryClient};

/// A server over an empty `VIRTX_HOME`, removed when this drops.
struct Server {
    client: Client,
    home: PathBuf,
    /// Whether the home is this server's to remove, rather than another's it shares.
    owns: bool,
}

/// Where a test pulls `reference` from: Docker Hub's official images from Google's mirror of
/// them, as Docker Hub refuses anonymous pulls past a few an hour, and AWS's mirror past one a
/// second -- a test failing on that says nothing of what it tries. Anything else from where it
/// names.
fn mirrored(reference: &str) -> String {
    let name = reference.strip_prefix("docker.io/").unwrap_or(reference);
    let name = name.strip_prefix("library/").unwrap_or(name);
    if name.contains('/') {
        return reference.to_owned();
    }
    format!("mirror.gcr.io/library/{name}")
}

impl Server {
    /// The directory of the client's own project, the one project here: what the tests reach,
    /// naming none. Asked of the server first, which makes it as it starts.
    async fn project(&self) -> PathBuf {
        self.client.list_volume().await.unwrap();
        std::fs::read_dir(self.home.join("project"))
            .unwrap()
            .flatten()
            .map(|project| project.path())
            .find(|project| project.is_dir())
            .expect("the client's own project")
    }

    /// Where the disk of sandbox `id` is: under its project's directory, whichever project of
    /// the home it is in, or else where none is.
    fn disk(&self, id: &str) -> PathBuf {
        let projects = std::fs::read_dir(self.home.join("project"))
            .into_iter()
            .flatten();
        projects
            .flatten()
            .map(|project| project.path().join(format!("sandbox/{id}.ext4")))
            .find(|disk| disk.is_file())
            .unwrap_or_else(|| self.home.join(format!("project/none/sandbox/{id}.ext4")))
    }

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
        // Gone before the next is spawned: its sandboxes parked, and kept.
        let nowhere = Client::connect("http://127.0.0.1:9", Auth::None, Default::default());
        let gone = std::mem::replace(&mut self.client, nowhere.unwrap());
        gone.close().await;
        let mut command = Command::new(env!("CARGO_BIN_EXE_virtx-stdio"));
        command.env("VIRTX_HOME", &self.home);
        self.client = Client::spawn(command).unwrap();
        self
    }

    /// Pull `reference` from its registry into the store the server reads, under its own name.
    async fn pull_image(&self, reference: &str) {
        let store = ImageStore::new(self.project().await.join("images")).unwrap();
        let named = reference.parse().unwrap();
        let reference = mirrored(reference).parse().unwrap();
        let registry = RegistryClient::new(store.clone(), None);
        let (_, _, manifest) = registry.pull_manifest(&reference).await.unwrap();
        let (_, config) = registry.pull_config(&reference, &manifest).await.unwrap();
        let mut layers = Vec::new();
        for descriptor in manifest.layers() {
            let tree = registry.pull_layer(&reference, descriptor).await.unwrap();
            layers.push(store.insert_layer(&tree).unwrap());
        }
        let image = store.insert_image(&config, &layers).unwrap();
        store.insert_tag(&named, &image).unwrap();
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
            // A job that failed ends with its error in place of its end entry.
            let entry = match entry {
                Ok(entry) => entry.entry,
                Err(virtx_core::Error::Job(why)) => {
                    let end = ImageJobEnd {
                        digest: None,
                        error: Some(why),
                    };
                    return (lines, end);
                }
                Err(e) => panic!("{e}"),
            };
            match entry {
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

/// A sandbox of `image` with 1 GiB of memory: enough for what these tests run in it, and little
/// enough that the suite's machines, booting at once, fit a host that commits each one's memory as
/// it boots (`VIRTX_PREFAULT_MEMORY`, as Windows' CI runs them), where 4 GiB each does not.
fn small(image: impl Into<ImageName>) -> SandboxCreateArgs {
    let mut args = SandboxCreateArgs::new(image);
    args.memory_mib = Some(1024);
    args
}

/// Boots VMs: needs the network to pull alpine, the binary signed for the hypervisor, and the
/// guest kernel where libkrun finds it.
#[tokio::test]
#[ignore]
async fn a_sandbox_runs_jobs_on_its_disk_and_volumes() {
    let server = Server::spawn("sandbox");
    server.pull_image("alpine:3.20").await;
    server.client.create_volume(Some("data")).await.unwrap();
    let mut args = small("alpine:3.20");
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
    assert!(listed[0].running);
    assert_eq!(listed[0].name.as_deref(), Some("work"));
    assert!(server.disk(&id).is_file());

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
        std::fs::read_to_string(server.project().await.join("volume/data/files/shared")).unwrap(),
        "shared\n"
    );
    let (lines, ended) = server.exec(&id, &["cat", "/root/kept"], None).await;
    assert_eq!((lines, ended.unwrap()), (vec!["kept".into()], 0));
    assert!(
        server
            .client
            .find_sandbox("work")
            .await
            .unwrap()
            .unwrap()
            .running
    );

    server.client.remove_sandbox(&id).await.unwrap();
    assert_eq!(status(server.client.remove_sandbox(&id).await), 404);
    assert!(!server.disk(&id).exists());
}

/// Boots VMs, as the one above does.
#[tokio::test]
#[ignore]
async fn a_sandbox_is_kept_across_a_server_started_again() {
    let server = Server::spawn("kept");
    server.pull_image("alpine:3.20").await;
    let mut args = small("alpine:3.20");
    args.name = Some("kept".into());
    args.ports = vec![8000.into()];
    let id = server.client.create_sandbox(&args).await.unwrap().id;
    let (_, ended) = server
        .exec(&id, &["sh", "-c", "echo kept > /root/kept"], None)
        .await;
    assert_eq!(ended.unwrap(), 0);

    // Its client gone, the server parks it, and the next one over the home knows it, running
    // still.
    let server = server.again().await;
    let found = server.client.find_sandbox("kept").await.unwrap().unwrap();
    assert_eq!((found.id.as_str(), found.running), (id.as_str(), true));

    // Booted by a job, as it was left.
    let (lines, ended) = server.exec(&id, &["cat", "/root/kept"], None).await;
    assert_eq!((lines, ended.unwrap()), (vec!["kept".into()], 0));
    server.client.remove_sandbox(&id).await.unwrap();
}

/// Boots VMs, as the one above does.
#[tokio::test]
#[ignore]
async fn a_sandbox_running_in_another_server_is_refused() {
    // Kept up a minute past its last use, so that it runs as the other server asks for it: this
    // server's default parks it at once.
    let server = Server::spawn_with("beside", &[("VIRTX_PARK_AFTER_MS", "60000")]);
    server.pull_image("alpine:3.20").await;
    let id = server
        .client
        .create_sandbox(&small("alpine:3.20"))
        .await
        .unwrap()
        .id;
    let (_, ended) = server
        .exec(&id, &["sh", "-c", "echo kept > /root/kept"], None)
        .await;
    assert_eq!(ended.unwrap(), 0);

    // Its disk is held by the machine the first runs: the second boots it not, for a job or
    // for its files, nor deletes it.
    let other = server.beside();
    assert!(other.client.get_sandbox(&id).await.unwrap().running);
    let refused = other
        .client
        .exec_sandbox(&id, &SandboxExecArgs::new(["true"]))
        .await;
    assert_eq!(status(refused), 409);
    assert_eq!(status(other.client.remove_sandbox(&id).await), 409);
    let root = FileRoot::Sandbox(id.clone());
    assert_eq!(
        status(other.client.read_text(&root, "root/kept").await),
        409
    );
    let (lines, ended) = server.exec(&id, &["cat", "/root/kept"], None).await;
    assert_eq!((lines, ended.unwrap()), (vec!["kept".into()], 0));

    // Parked there, its client gone, it is the other's to boot.
    let _server = server.again().await;
    let (lines, ended) = other.exec(&id, &["cat", "/root/kept"], None).await;
    assert_eq!((lines, ended.unwrap()), (vec!["kept".into()], 0));
    other.client.remove_sandbox(&id).await.unwrap();
}

/// Boots VMs, as the one above does.
#[tokio::test]
#[ignore]
async fn an_unused_sandbox_is_parked_and_booted_again_with_what_it_wrote() {
    let server = Server::spawn_with("park", &[("VIRTX_PARK_AFTER_MS", "500")]);
    server.pull_image("alpine:3.20").await;
    let id = server
        .client
        .create_sandbox(&small("alpine:3.20"))
        .await
        .unwrap()
        .id;
    let root = FileRoot::Sandbox(id.clone());
    // What runs past its job ends with the machine, so seeing it gone is seeing it parked.
    let behind = ["sh", "-c", "sleep 1000 >/dev/null 2>&1 &"];
    let still = ["sh", "-c", "cat /root/kept; ps | grep -c '[s]leep 1000'"];

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

    // Unused, it is parked, running still; a request to its files boots it, over what it
    // wrote.
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    assert!(server.client.get_sandbox(&id).await.unwrap().running);
    assert_eq!(
        server.client.read_text(&root, "root/kept").await.unwrap(),
        "kept\n"
    );
    let (lines, _) = server.exec(&id, &still, None).await;
    assert_eq!(lines, ["kept", "0"]);

    // What a request to its files wrote is there after it is parked again, and a job boots it.
    server
        .client
        .write_file(&root, "root/written", "written\n", None)
        .await
        .unwrap();
    let (_, ended) = server.exec(&id, &behind, None).await;
    assert_eq!(ended.unwrap(), 0);
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    let (lines, _) = server
        .exec(
            &id,
            &["sh", "-c", "cat /root/written; ps | grep -c '[s]leep 1000'"],
            None,
        )
        .await;
    assert_eq!(lines, ["written", "0"]);

    server.client.remove_sandbox(&id).await.unwrap();
    let left = std::fs::read_dir(server.project().await.join("sandbox"))
        .unwrap()
        .count();
    assert_eq!(left, 0);
}

/// Boots VMs, as the one above does. A sandbox parked once unused has all it wrote on its
/// disk, left in the guest's page cache as it was.
#[tokio::test]
#[ignore]
async fn a_sandbox_parked_unused_has_synced_what_it_wrote() {
    let server = Server::spawn_with("park-synced", &[("VIRTX_PARK_AFTER_MS", "500")]);
    server.pull_image("alpine:3.20").await;
    let id = server
        .client
        .create_sandbox(&small("alpine:3.20"))
        .await
        .unwrap()
        .id;
    let (_, ended) = server
        .exec(
            &id,
            &["dd", "if=/dev/zero", "of=/root/big", "bs=1M", "count=64"],
            None,
        )
        .await;
    assert_eq!(ended.unwrap(), 0);

    // Read from its disk until it is all there, which it is once its machine is parked: a
    // guard against a hang, not a measure of how soon.
    let size = || {
        let disk = std::fs::File::open(server.disk(&id)).ok()?;
        match FsTree::from_ext4(disk)
            .ok()?
            .entry(b"upper/root/big")
            .ok()?
        {
            Some(Entry::File { data, .. }) => Some(data.len()),
            _ => None,
        }
    };
    let synced = tokio::time::timeout(std::time::Duration::from_secs(60), async {
        while size() != Some(64 << 20) {
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
    })
    .await;
    assert!(
        synced.is_ok(),
        "its disk never held all it wrote: {:?}",
        size()
    );
    server.client.remove_sandbox(&id).await.unwrap();
}

/// Boots VMs, as the one above does.
#[tokio::test]
#[ignore]
async fn a_job_s_stdin_is_written_and_its_log_answers_its_exec() {
    let server = Server::spawn("stdin");
    server.pull_image("alpine:3.20").await;
    let id = server
        .client
        .create_sandbox(&small("alpine:3.20"))
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
    let mut args = small("alpine:3.20");
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
    let mut args = small("alpine:3.20");
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
async fn a_disk_a_parked_machine_left_reads_as_its_kernel_committed() {
    let server = Server::spawn("killed");
    server.pull_image("alpine:3.20").await;
    let args = small("alpine:3.20");
    let id = server.client.create_sandbox(&args).await.unwrap().id;

    // Synced, so committed, and then parked with its disk still mounted.
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
    // Parked as its client goes.
    let server = server.again().await;

    let path = server.disk(&id);
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
    let (lines, ended) = server.exec(&id, &["cat", "/root/kept/100"], None).await;
    assert_eq!((lines, ended.unwrap()), (vec!["100".into()], 0));
}

/// Boots VMs, as the one above does.
#[tokio::test]
#[ignore]
async fn a_built_image_runs_with_what_its_steps_made() {
    let server = Server::spawn("build");
    // The server pulls it itself: from the mirror, by name ([`mirrored`]).
    let recipe = Recipe::new(mirrored("alpine:3.20"))
        .step("apk add --no-cache jq")
        .step(Step::env("GREETING", "hello"))
        .step(Step::workdir("/work"))
        .step("echo \"$GREETING\" > note && rm /etc/motd");
    let (lines, built) = server.build(recipe, "built:latest").await;
    let digest = built.digest.unwrap_or_else(|| panic!("{:?}", built.error));
    assert!(
        lines.contains(&format!("pulling {}", mirrored("alpine:3.20"))),
        "{lines:?}"
    );
    assert!(lines.contains(&"STEP 4/4: RUN echo \"$GREETING\" > note && rm /etc/motd".to_string()));

    let id = server
        .client
        .create_sandbox(&small("built:latest"))
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
    let by_digest = small(ImageName::Digest { digest });
    assert!(server.client.create_sandbox(&by_digest).await.is_ok());

    // A step that fails fails the build, and names nothing.
    let (_, failed) = server
        .build(Recipe::new("built:latest").step("exit 3"), "failed:latest")
        .await;
    assert!(failed.error.is_some(), "{failed:?}");
    let created = server.client.create_sandbox(&small("failed:latest")).await;
    assert_eq!(status(created), 404);
}

/// Boots VMs, as the one above does.
#[tokio::test]
#[ignore]
async fn a_cancelled_build_takes_its_machine_with_it() {
    let server = Server::spawn("cancel");
    let recipe = Recipe::new(mirrored("alpine:3.20")).step("echo started; sleep infinity");
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
    // A machine's process names the server's home on its command line, in its rootfs's path:
    // on Windows, its name, as the path there is spelled with its backslashes escaped.
    let running = || {
        #[cfg(unix)]
        let looked = std::process::Command::new("pgrep")
            .arg("-f")
            .arg(&server.home)
            .stdout(std::process::Stdio::null())
            .status();
        #[cfg(windows)]
        let looked = std::process::Command::new("powershell")
            .args(["-NoProfile", "-Command"])
            .arg(format!(
                "if (Get-CimInstance Win32_Process | Where-Object {{ $_.ProcessId -ne $PID -and \
                 $_.CommandLine -like '*{}*' }}) {{ exit 0 }} else {{ exit 1 }}",
                server.home.file_name().unwrap().to_str().unwrap()
            ))
            .stdout(std::process::Stdio::null())
            .status();
        looked.unwrap().success()
    };
    assert!(running());

    server.client.remove_image_job(&job.id).await.unwrap();
    // Looked for a minute: what is tried is that it goes, not how soon.
    let until = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while std::time::Instant::now() < until {
        if !running() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("the build's machine outlived its job");
}

/// A pull's digest is the image it tagged here, not the registry's index it chose from.
/// Needs the network to pull alpine.
#[tokio::test]
#[ignore]
async fn a_pull_answers_the_image_it_tagged() {
    let server = Server::spawn("pull");
    let job = server
        .client
        .pull_image(&mirrored("alpine:3.20"), None)
        .await
        .unwrap();
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
