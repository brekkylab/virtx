//! End to end: the typed client against a server across a real transport, each scenario a flow
//! of requests over images, snapshots, volumes, sandboxes and their jobs.
//!
//! The server is the [`testing`](virtx_core::testing) one: by default this test binary, run
//! again as a child process over stdio, or with `VIRTX_E2E_TCP=1` served on the loopback over
//! TCP. `VIRTX_E2E_SERVER` names a server to spawn instead, its arguments split on whitespace,
//! and `VIRTX_E2E_URL` one to connect to, with `VIRTX_E2E_TOKEN` its bearer token. Against those
//! only the scenarios every server passes run: they assert nothing `PROTOCOL.md` leaves to the
//! server, run no command but `echo`, `cat` and `sleep`, and name what they make uniquely.
//!
//! Against another server, what a scenario made is removed once it ends, passed or not, and a
//! `sleep` it started is killed at its timeout if it is not. Each name it removes is its own,
//! `e2e-...`; the one thing it changes that is not is the tag `alpine:3.20`, which a pull moves
//! to what its registry has now. A prune deletes every image nothing holds, so the scenario that
//! prunes runs against another server only with `VIRTX_E2E_ALLOW_PRUNE=1`.
//!
//! Without libtest's harness, which writes its own lines to stdout: over stdio, stdout is the
//! server's HTTP/2 connection and nothing else. An argument runs the scenarios whose names hold
//! it.

use std::{
    collections::BTreeMap,
    future::Future,
    panic::{self, AssertUnwindSafe},
    path::PathBuf,
    pin::Pin,
    process::ExitCode,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use futures_util::{Stream, StreamExt, TryStreamExt};
use http::HeaderMap;
use hyper::service::Service as _;
use hyper_util::service::TowerToHyperService;
use virtx_core::{
    Auth, Client, Error, Event, FileRoot, ImageJobEnd, ImageJobEntry, LogEntry, MountSource,
    MountType, Recipe, RegistryAuth, Sandbox, SandboxCreateArgs, SandboxExecArgs, SandboxJobEnd,
    SandboxJobEntry, SandboxVolume, Step, VolumeMountListOutputItem, serve_stdio,
    testing::{LOG_LIMIT, router},
};

/// Set in the environment of this binary when it runs as the server.
const SERVE: &str = "VIRTX_E2E_SERVE";

/// The image every scenario that boots a sandbox pulls first.
const ALPINE: &str = "alpine:3.20";

/// The server each scenario gets a client of.
#[derive(Clone)]
enum Server {
    /// The test server, a child process of this binary over stdio.
    Test,
    /// The test server, served on the loopback over TCP.
    TestTcp,
    /// A server spawned from this argv.
    Spawn(Vec<String>),
    /// A server at this URL, and the credentials it wants.
    Url(String, Auth),
}

impl Server {
    fn from_env() -> Server {
        let var = |name| std::env::var(name).ok().filter(|v| !v.is_empty());
        if let Some(command) = var("VIRTX_E2E_SERVER") {
            return Server::Spawn(command.split_whitespace().map(Into::into).collect());
        }
        if let Some(url) = var("VIRTX_E2E_URL") {
            let auth = match var("VIRTX_E2E_TOKEN") {
                Some(token) => Auth::Bearer { token },
                None => Auth::None,
            };
            return Server::Url(url, auth);
        }
        if var("VIRTX_E2E_TCP").is_some() {
            return Server::TestTcp;
        }
        Server::Test
    }

    /// Whether it is the test server, whose pretending a scenario may lean on.
    fn is_test(&self) -> bool {
        matches!(self, Server::Test | Server::TestTcp)
    }

    /// Whether each client has a server process of its own, which closing it ends.
    fn is_spawned(&self) -> bool {
        matches!(self, Server::Test | Server::Spawn(_))
    }

    /// A client of a server of its own, or of the one at the URL.
    async fn client(&self) -> Client {
        match self {
            Server::Test => {
                let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
                command.env(SERVE, "1");
                Client::spawn(command).unwrap()
            }
            Server::TestTcp => {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let url = format!("http://{}/", listener.local_addr().unwrap());
                tokio::spawn(async move { axum::serve(listener, router()).await });
                Client::connect(&url, Auth::None, HeaderMap::new()).unwrap()
            }
            Server::Spawn(argv) => {
                let mut command = tokio::process::Command::new(&argv[0]);
                command.args(&argv[1..]);
                Client::spawn(command).unwrap()
            }
            Server::Url(url, auth) => Client::connect(url, auth.clone(), HeaderMap::new()).unwrap(),
        }
    }
}

/// What a scenario made on the server, to remove once it ends.
enum Made {
    Sandbox(String),
    Volume(String),
    Snapshot(String),
    Tag(String),
}

/// One scenario's server, and what the scenario has made there.
#[derive(Clone)]
struct Run {
    server: Server,
    made: Arc<Mutex<Vec<Made>>>,
}

impl Run {
    async fn client(&self) -> Client {
        self.server.client().await
    }

    fn is_test(&self) -> bool {
        self.server.is_test()
    }

    /// Keep `made` to remove once the scenario ends. A name is the scenario's own, from
    /// [`unique`], so what is removed is never what was there before it.
    fn made(&self, made: Made) {
        if let Made::Volume(name) | Made::Snapshot(name) | Made::Tag(name) = &made {
            assert!(name.starts_with("e2e-"), "{name} is not a scenario's own");
        }
        self.made.lock().unwrap().push(made);
    }

    async fn create_sandbox(&self, client: &Client, args: &SandboxCreateArgs) -> Sandbox {
        let sandbox = client.create_sandbox(args).await.unwrap();
        self.made(Made::Sandbox(sandbox.id.clone()));
        sandbox
    }

    /// Remove what the scenario made and did not remove, whether it passed or not: sandboxes
    /// first, since each holds its snapshot and the volumes it mounts. What is gone already
    /// is passed over.
    async fn clean(&self, client: &Client) {
        let mut made = std::mem::take(&mut *self.made.lock().unwrap());
        made.sort_by_key(|made| match made {
            Made::Sandbox(_) => 0,
            Made::Volume(_) => 1,
            Made::Snapshot(_) => 2,
            Made::Tag(_) => 3,
        });
        for made in made {
            let _ = match &made {
                Made::Sandbox(id) => client.remove_sandbox(id).await,
                Made::Volume(name) => client.remove_volume(name).await,
                Made::Snapshot(name) => client.remove_snapshot(name).await,
                Made::Tag(reference) => client.remove_image(reference).await,
            };
        }
    }
}

/// A name no other scenario, or run, gives: `e2e-<what>-<8 hex digits>`.
fn unique(what: &str) -> String {
    let id = uuid::Uuid::new_v4().simple().to_string();
    format!("e2e-{what}-{}", &id[..8])
}

/// The status and the `code` `result` was refused with.
fn refused<T: std::fmt::Debug>(result: Result<T, Error>) -> (u16, String) {
    match result {
        Err(Error::Status(status, message)) => {
            let code = message.split(':').next().unwrap_or("").to_owned();
            (status, code)
        }
        other => panic!("not a refusal: {other:?}"),
    }
}

/// Every tag on the server's images, as `name:tag`, with the digest it points at.
async fn tags(client: &Client) -> BTreeMap<String, String> {
    let listed = client.list_image().await.unwrap();
    listed
        .into_iter()
        .map(|image| (image.reference, image.digest))
        .collect()
}

/// How the image job `id` ended, once it has.
async fn image_end(client: &Client, id: &str) -> ImageJobEnd {
    let log: Vec<_> = client.image_job_log(id, None).try_collect().await.unwrap();
    match log.into_iter().last().map(|entry| entry.entry) {
        Some(ImageJobEntry::End(end)) => end,
        other => panic!("the log of {id} ends with {other:?}"),
    }
}

/// The digest the image job `id` ended with, which it fails without.
async fn image_digest(client: &Client, id: &str) -> String {
    let end = image_end(client, id).await;
    end.digest
        .unwrap_or_else(|| panic!("{id} failed: {:?}", end.error))
}

/// [`ALPINE`] pulled, and its digest.
async fn alpine(client: &Client) -> String {
    let job = client.pull_image(ALPINE, None).await.unwrap();
    image_digest(client, &job.id).await
}

/// `reference` built from [`ALPINE`] with a step of its own, so its image is no other's, and
/// its digest.
async fn build(run: &Run, client: &Client, reference: &str) -> String {
    run.made(Made::Tag(reference.into()));
    let recipe = Recipe::new(ALPINE).step(Step::run(format!("echo {reference} > /built")));
    let job = client.build_image(recipe, reference).await.unwrap();
    image_digest(client, &job.id).await
}

/// What a stream of the client's own, such as a download, gave last.
async fn output<T>(events: impl Stream<Item = Result<Event<T, String>, Error>> + Unpin) -> T {
    let mut events = events;
    while let Some(event) = events.next().await {
        if let Event::Output(output) = event.unwrap() {
            return output;
        }
    }
    panic!("a stream ended without its output")
}

/// A path in the system's temporary directory no other scenario uses.
fn temporary(name: &str) -> PathBuf {
    std::env::temp_dir().join(unique(name))
}

fn exec(cmd: &[&str]) -> SandboxExecArgs {
    SandboxExecArgs::new(cmd.iter().copied())
}

/// A `sleep` that outlives no scenario: killed at its timeout if the scenario dies first.
fn sleep() -> SandboxExecArgs {
    SandboxExecArgs {
        timeout_ms: Some(120_000),
        ..exec(&["sleep", "1000"])
    }
}

/// The entries of the job `job`'s log in `sandbox`, to its end.
async fn sandbox_log(client: &Client, sandbox: &str, job: &str) -> Vec<SandboxJobEntry> {
    client
        .sandbox_job_log(sandbox, job, None)
        .map_ok(|entry| entry.entry)
        .try_collect()
        .await
        .unwrap()
}

fn end(code: i32) -> SandboxJobEntry {
    SandboxJobEntry::End(SandboxJobEnd { code })
}

/// A sandbox made from `args`, of [`ALPINE`] pulled first.
async fn sandbox(run: &Run, client: &Client, args: SandboxCreateArgs) -> String {
    alpine(client).await;
    run.create_sandbox(client, &args).await.id
}

async fn a_pull_ends_with_the_digest_its_reference_points_at(run: Run) {
    let client = run.client().await;
    let job = client.pull_image(ALPINE, None).await.unwrap();
    let digest = image_end(&client, &job.id).await.digest.expect("pulled");
    let read = client.get_image_job(&job.id).await.unwrap();
    assert!(!read.running);
    assert_eq!(read.digest.as_ref(), Some(&digest));
    assert_eq!(tags(&client).await.get(ALPINE), Some(&digest));
    client.close().await;
}

async fn a_build_points_its_reference_at_what_it_built(run: Run) {
    let client = run.client().await;
    let reference = format!("{}:latest", unique("build"));
    let digest = build(&run, &client, &reference).await;
    assert_eq!(tags(&client).await.get(&reference), Some(&digest));
    client.remove_image(&reference).await.unwrap();
    client.close().await;
}

async fn a_sandbox_boots_the_image_a_build_tagged(run: Run) {
    let client = run.client().await;
    let reference = format!("{}:latest", unique("boot"));
    let digest = build(&run, &client, &reference).await;
    let sandbox = run
        .create_sandbox(&client, &SandboxCreateArgs::new(reference.as_str()))
        .await;
    assert_eq!(sandbox.image, digest);
    let job = client
        .exec_sandbox(&sandbox.id, &exec(&["echo", "hi"]))
        .await
        .unwrap();
    assert_eq!(
        sandbox_log(&client, &sandbox.id, &job.id).await,
        [SandboxJobEntry::Stdout("hi".into()), end(0)]
    );
    client.remove_sandbox(&sandbox.id).await.unwrap();
    client.remove_image(&reference).await.unwrap();
    client.close().await;
}

async fn a_sandbox_of_an_image_not_there_is_refused(run: Run) {
    let client = run.client().await;
    let missing = format!("{}:latest", unique("missing"));
    let made = client
        .create_sandbox(&SandboxCreateArgs::new(missing.as_str()))
        .await;
    if let Ok(sandbox) = &made {
        run.made(Made::Sandbox(sandbox.id.clone()));
    }
    assert_eq!(refused(made), (404, "not_found".into()));
    client.close().await;
}

async fn an_image_downloaded_uploads_back_as_it_was(run: Run) {
    let client = run.client().await;
    let digest = alpine(&client).await;
    let reference = format!("{}:copy", unique("copy"));
    run.made(Made::Tag(reference.clone()));
    client.tag_image(ALPINE, &reference).await.unwrap();
    let path = temporary("image.tar");

    let downloaded = output(client.download_image(&reference, &path)).await;
    assert_eq!(downloaded.digest, digest);
    client.remove_image(&reference).await.unwrap();
    assert!(!tags(&client).await.contains_key(&reference));

    let uploaded = output(client.upload_image(&path, &reference)).await;
    assert_eq!(uploaded.digest, digest);
    assert_eq!(tags(&client).await.get(&reference), Some(&digest));

    client.remove_image(&reference).await.unwrap();
    std::fs::remove_file(&path).unwrap();
    client.close().await;
}

async fn untagging_takes_that_tag_alone(run: Run) {
    let client = run.client().await;
    let digest = alpine(&client).await;
    let name = unique("tag");
    let (kept, untagged) = (format!("{name}:kept"), format!("{name}:untagged"));
    run.made(Made::Tag(kept.clone()));
    run.made(Made::Tag(untagged.clone()));
    client.tag_image(ALPINE, &kept).await.unwrap();
    client.tag_image(ALPINE, &untagged).await.unwrap();

    client.untag_image(&untagged).await.unwrap();
    let tags = tags(&client).await;
    assert_eq!(tags.get(&kept), Some(&digest));
    assert_eq!(tags.get(ALPINE), Some(&digest));
    assert!(!tags.contains_key(&untagged));
    client.untag_image(&kept).await.unwrap();
    client.close().await;
}

async fn a_prune_keeps_what_a_tag_or_a_sandbox_holds(run: Run) {
    let client = run.client().await;
    let alpine = alpine(&client).await;
    let (loose, booted) = (
        format!("{}:latest", unique("loose")),
        format!("{}:latest", unique("booted")),
    );
    let loose_digest = build(&run, &client, &loose).await;
    let booted_digest = build(&run, &client, &booted).await;
    let sandbox = run
        .create_sandbox(&client, &SandboxCreateArgs::new(booted.as_str()))
        .await;
    client.untag_image(&loose).await.unwrap();
    client.untag_image(&booted).await.unwrap();

    let pruned = client.prune_image().await.unwrap();
    assert!(!pruned.images.contains(&alpine), "{pruned:?}");
    assert!(!pruned.images.contains(&booted_digest), "{pruned:?}");
    // A server may leave what is too new to tell from what a job will tag: only the test
    // server is sure to take it now.
    if run.is_test() {
        assert!(pruned.images.contains(&loose_digest), "{pruned:?}");
        assert!(pruned.reclaimed > 0);
    }

    client.remove_sandbox(&sandbox.id).await.unwrap();
    if run.is_test() {
        let pruned = client.prune_image().await.unwrap();
        assert_eq!(pruned.images, [booted_digest]);
    }
    assert_eq!(tags(&client).await.get(ALPINE), Some(&alpine));
    client.close().await;
}

async fn a_push_ends_with_the_digest_it_sent_and_shows_no_credentials(run: Run) {
    let client = run.client().await;
    let digest = alpine(&client).await;
    let auth = RegistryAuth::Bearer {
        token: "e2e-secret".into(),
    };
    let job = client.push_image(ALPINE, Some(auth)).await.unwrap();
    let log: Vec<_> = client
        .image_job_log(&job.id, None)
        .try_collect()
        .await
        .unwrap();
    assert_eq!(
        log.last().map(|entry| &entry.entry),
        Some(&ImageJobEntry::End(ImageJobEnd {
            digest: Some(digest),
            error: None,
        }))
    );
    let shown = serde_json::to_string(&(client.get_image_job(&job.id).await.unwrap(), log));
    assert!(!shown.unwrap().contains("e2e-secret"));

    let missing = format!("{}:latest", unique("unpushed"));
    let job = client.push_image(&missing, None).await.unwrap();
    assert!(image_end(&client, &job.id).await.error.is_some());
    client.close().await;
}

async fn a_sandbox_holds_its_snapshot_until_it_is_removed(run: Run) {
    let client = run.client().await;
    let name = unique("snapshot");
    run.made(Made::Snapshot(name.clone()));
    let snapshot = client.create_snapshot(Some(&name), None).await.unwrap();
    assert!(!snapshot.in_use);
    let id = sandbox(
        &run,
        &client,
        SandboxCreateArgs {
            snapshot: Some(name.clone()),
            ..SandboxCreateArgs::new(ALPINE)
        },
    )
    .await;
    assert!(client.get_snapshot(&name).await.unwrap().in_use);
    assert_eq!(
        refused(client.remove_snapshot(&name).await),
        (409, "in_use".into())
    );
    let root = FileRoot::Snapshot(name.clone());
    assert!(matches!(
        client.read_dir(&root, "").await,
        Err(Error::Status(423, _))
    ));

    client.remove_sandbox(&id).await.unwrap();
    assert!(!client.get_snapshot(&name).await.unwrap().in_use);
    client.read_dir(&root, "").await.unwrap();
    client.remove_snapshot(&name).await.unwrap();
    assert_eq!(refused(client.get_snapshot(&name).await).0, 404);
    client.close().await;
}

async fn a_sandbox_holds_the_volumes_it_mounts_until_it_is_removed(run: Run) {
    let client = run.client().await;
    let volume = unique("volume");
    run.made(Made::Volume(volume.clone()));
    client.create_volume(Some(&volume)).await.unwrap();
    let root = FileRoot::Volume(volume.clone());
    client
        .write_file(&root, "kept.txt", "kept", None)
        .await
        .unwrap();
    let mount = |path: &str| SandboxVolume {
        volume: volume.clone(),
        path: path.into(),
        readonly: false,
    };

    let twice = SandboxCreateArgs {
        volumes: vec![mount("/data"), mount("/data")],
        ..SandboxCreateArgs::new(ALPINE)
    };
    alpine(&client).await;
    let made = client.create_sandbox(&twice).await;
    if let Ok(sandbox) = &made {
        run.made(Made::Sandbox(sandbox.id.clone()));
    }
    let (status, _) = refused(made);
    assert!((400..500).contains(&status), "{status}");

    let id = sandbox(
        &run,
        &client,
        SandboxCreateArgs {
            volumes: vec![mount("/data")],
            ..SandboxCreateArgs::new(ALPINE)
        },
    )
    .await;
    assert_eq!(
        refused(client.remove_volume(&volume).await),
        (409, "in_use".into())
    );
    client.remove_sandbox(&id).await.unwrap();
    assert_eq!(client.read_text(&root, "kept.txt").await.unwrap(), "kept");
    client.remove_volume(&volume).await.unwrap();
    assert_eq!(refused(client.get_volume(&volume).await).0, 404);
    client.close().await;
}

async fn a_process_writes_its_output_then_ends(run: Run) {
    let client = run.client().await;
    let id = sandbox(&run, &client, SandboxCreateArgs::new(ALPINE)).await;
    let job = client
        .exec_sandbox(&id, &exec(&["echo", "a", "b"]))
        .await
        .unwrap();
    assert_eq!(
        sandbox_log(&client, &id, &job.id).await,
        [SandboxJobEntry::Stdout("a b".into()), end(0)]
    );
    let read = client.get_sandbox_job(&id, &job.id).await.unwrap();
    assert!(!read.running);
    assert_eq!(read.code, Some(0));
    assert_eq!(read.cmd, ["echo", "a", "b"]);
    client.remove_sandbox(&id).await.unwrap();
    client.close().await;
}

async fn stdin_written_comes_back_until_it_is_closed(run: Run) {
    let client = run.client().await;
    let id = sandbox(&run, &client, SandboxCreateArgs::new(ALPINE)).await;
    let args = SandboxExecArgs {
        stdin: true,
        ..exec(&["cat"])
    };
    let job = client.exec_sandbox(&id, &args).await.unwrap();
    client
        .write_sandbox_job_stdin(&id, &job.id, Some("one\ntwo\n"), false)
        .await
        .unwrap();
    client
        .write_sandbox_job_stdin(&id, &job.id, None::<String>, true)
        .await
        .unwrap();
    assert_eq!(
        sandbox_log(&client, &id, &job.id).await,
        [
            SandboxJobEntry::Stdout("one".into()),
            SandboxJobEntry::Stdout("two".into()),
            end(0)
        ]
    );
    let again = client
        .write_sandbox_job_stdin(&id, &job.id, Some("three\n"), false)
        .await;
    assert_eq!(refused(again), (409, "ended".into()));
    client.remove_sandbox(&id).await.unwrap();
    client.close().await;
}

async fn a_signal_a_timeout_or_a_removal_ends_a_process(run: Run) {
    let client = run.client().await;
    let id = sandbox(&run, &client, SandboxCreateArgs::new(ALPINE)).await;
    let sleep = sleep();

    let signalled = client.exec_sandbox(&id, &sleep).await.unwrap();
    client
        .signal_sandbox_job(&id, &signalled.id, "SIGTERM")
        .await
        .unwrap();
    let log = sandbox_log(&client, &id, &signalled.id).await;
    assert_eq!(log.last(), Some(&end(128 + 15)));
    let again = client
        .signal_sandbox_job(&id, &signalled.id, "SIGTERM")
        .await;
    assert_eq!(refused(again), (409, "ended".into()));

    let timed = SandboxExecArgs {
        timeout_ms: Some(200),
        ..sleep.clone()
    };
    let timed = client.exec_sandbox(&id, &timed).await.unwrap();
    let log = sandbox_log(&client, &id, &timed.id).await;
    assert_eq!(log.last(), Some(&end(128 + 9)));

    let removed = client.exec_sandbox(&id, &sleep).await.unwrap();
    client.remove_sandbox_job(&id, &removed.id).await.unwrap();
    assert_eq!(
        refused(client.get_sandbox_job(&id, &removed.id).await).0,
        404
    );
    client.remove_sandbox(&id).await.unwrap();
    client.close().await;
}

async fn processes_in_a_sandbox_run_side_by_side(run: Run) {
    let client = run.client().await;
    let id = sandbox(&run, &client, SandboxCreateArgs::new(ALPINE)).await;
    let sleep = sleep();
    let first = client.exec_sandbox(&id, &sleep).await.unwrap();
    let second = client.exec_sandbox(&id, &sleep).await.unwrap();
    let echo = client
        .exec_sandbox(&id, &exec(&["echo", "beside"]))
        .await
        .unwrap();
    assert_eq!(
        sandbox_log(&client, &id, &echo.id).await,
        [SandboxJobEntry::Stdout("beside".into()), end(0)]
    );
    let listed: Vec<_> = client
        .list_sandbox_job(&id)
        .await
        .unwrap()
        .into_iter()
        .map(|job| job.id)
        .collect();
    for job in [&first.id, &second.id, &echo.id] {
        assert!(listed.contains(job), "{listed:?}");
    }
    for job in [&first.id, &second.id] {
        assert!(client.get_sandbox_job(&id, job).await.unwrap().running);
    }
    client.remove_sandbox(&id).await.unwrap();
    client.close().await;
}

async fn a_log_read_again_starts_after_the_entry_it_names(run: Run) {
    let client = run.client().await;
    let id = sandbox(&run, &client, SandboxCreateArgs::new(ALPINE)).await;
    let job = client
        .exec_sandbox(&id, &exec(&["echo", "hi"]))
        .await
        .unwrap();
    let whole: Vec<LogEntry<SandboxJobEntry>> = client
        .sandbox_job_log(&id, &job.id, None)
        .try_collect()
        .await
        .unwrap();
    assert_eq!(whole.len(), 2, "{whole:?}");
    let rest: Vec<_> = client
        .sandbox_job_log(&id, &job.id, Some(whole[0].id))
        .try_collect()
        .await
        .unwrap();
    assert_eq!(rest, whole[1..]);
    client.remove_sandbox(&id).await.unwrap();
    client.close().await;
}

async fn a_command_that_cannot_start_makes_no_job(run: Run) {
    let client = run.client().await;
    let id = sandbox(&run, &client, SandboxCreateArgs::new(ALPINE)).await;
    let started = client
        .exec_sandbox(&id, &exec(&["e2e-no-such-command"]))
        .await;
    assert_eq!(refused(started), (422, "not_started".into()));
    assert!(client.list_sandbox_job(&id).await.unwrap().is_empty());
    client.remove_sandbox(&id).await.unwrap();
    client.close().await;
}

async fn removing_a_sandbox_ends_its_jobs_and_the_logs_read(run: Run) {
    let client = run.client().await;
    let id = sandbox(&run, &client, SandboxCreateArgs::new(ALPINE)).await;
    let job = client.exec_sandbox(&id, &sleep()).await.unwrap();
    // The log is being read when the sandbox goes: its stream ends, whatever it ends with.
    let reading = client
        .sandbox_job_log(&id, &job.id, None)
        .collect::<Vec<_>>();
    let removing = async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        client.remove_sandbox(&id).await
    };
    let (_, removed) = tokio::join!(reading, removing);
    removed.unwrap();
    assert_eq!(refused(client.get_sandbox(&id).await).0, 404);
    assert_eq!(refused(client.get_sandbox_job(&id, &job.id).await).0, 404);
    client.close().await;
}

async fn closing_the_client_ends_its_server(run: Run) {
    let client = run.client().await;
    sandbox(&run, &client, SandboxCreateArgs::new(ALPINE)).await;
    let started = Instant::now();
    client.close().await;
    // The server exits once its stdin closes, well before it would be killed.
    assert!(started.elapsed() < Duration::from_secs(5));
}

async fn a_cancelled_image_job_changes_no_tag(run: Run) {
    let client = run.client().await;
    let before = tags(&client).await;
    let job = client.pull_image("hang", None).await.unwrap();
    assert!(job.running);
    client.remove_image_job(&job.id).await.unwrap();
    assert_eq!(refused(client.get_image_job(&job.id).await).0, 404);
    assert_eq!(tags(&client).await, before);
    client.close().await;
}

async fn a_log_past_the_limit_is_truncated_but_keeps_its_end(run: Run) {
    let client = run.client().await;
    let id = sandbox(&run, &client, SandboxCreateArgs::new(ALPINE)).await;
    let job = client
        .exec_sandbox(&id, &exec(&["lines", "20"]))
        .await
        .unwrap();
    let log = sandbox_log(&client, &id, &job.id).await;
    assert_eq!(log.len(), LOG_LIMIT + 2, "{log:?}");
    assert_eq!(log[LOG_LIMIT], SandboxJobEntry::Truncated {});
    assert_eq!(log.last(), Some(&end(0)));
    client.close().await;
}

async fn a_mount_shows_its_type_and_not_its_credentials(run: Run) {
    let client = run.client().await;
    let volume = unique("mounted");
    run.made(Made::Volume(volume.clone()));
    client.create_volume(Some(&volume)).await.unwrap();
    let source = || MountSource::Notion {
        api_key: "e2e-secret".into(),
    };
    let mount = VolumeMountListOutputItem {
        path: "notion".into(),
        mount_type: MountType::Notion,
    };
    let made = client
        .create_volume_mount(&volume, "notion", source())
        .await
        .unwrap();
    assert_eq!(made, mount);
    let listed = client.list_volume_mount(&volume).await.unwrap();
    assert_eq!(listed, [mount]);
    assert!(
        !serde_json::to_string(&listed)
            .unwrap()
            .contains("e2e-secret")
    );
    let again = client
        .create_volume_mount(&volume, "notion", source())
        .await;
    assert_eq!(refused(again), (409, "exists".into()));
    client.remove_volume_mount(&volume, "notion").await.unwrap();
    assert!(client.list_volume_mount(&volume).await.unwrap().is_empty());
    client.remove_volume(&volume).await.unwrap();
    client.close().await;
}

/// What a scenario needs of the server to run.
#[derive(Clone, Copy)]
enum Needs {
    /// Any server.
    Any,
    /// The test server, whose pretending it leans on.
    Test,
    /// A server process of its own.
    Spawned,
    /// The test server, or one `VIRTX_E2E_ALLOW_PRUNE=1` lets it prune: a prune deletes every
    /// image nothing holds, not only the scenario's.
    Prune,
}

type Scenario = fn(Run) -> Pin<Box<dyn Future<Output = ()>>>;

macro_rules! scenarios {
    ($($needs:ident $scenario:ident),* $(,)?) => {
        [$((
            stringify!($scenario),
            Needs::$needs,
            (|run| -> Pin<Box<dyn Future<Output = ()>>> { Box::pin($scenario(run)) })
                as Scenario,
        )),*]
    };
}

fn main() -> ExitCode {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    if std::env::var_os(SERVE).is_some() {
        let service = TowerToHyperService::new(router());
        let handle = move |request| {
            let service = service.clone();
            async move { service.call(request).await.unwrap_or_else(|e| match e {}) }
        };
        let served = serve_stdio(handle, tokio::io::stdin(), tokio::io::stdout());
        runtime.block_on(served).unwrap();
        return ExitCode::SUCCESS;
    }

    let server = Server::from_env();
    let filters: Vec<String> = std::env::args()
        .skip(1)
        .filter(|arg| !arg.starts_with('-'))
        .collect();
    // A real server builds and pulls for real.
    let limit = Duration::from_secs(if server.is_test() { 30 } else { 900 });
    let scenarios = scenarios![
        Any a_pull_ends_with_the_digest_its_reference_points_at,
        Any a_build_points_its_reference_at_what_it_built,
        Any a_sandbox_boots_the_image_a_build_tagged,
        Any a_sandbox_of_an_image_not_there_is_refused,
        Any an_image_downloaded_uploads_back_as_it_was,
        Any untagging_takes_that_tag_alone,
        Prune a_prune_keeps_what_a_tag_or_a_sandbox_holds,
        Any a_sandbox_holds_its_snapshot_until_it_is_removed,
        Any a_sandbox_holds_the_volumes_it_mounts_until_it_is_removed,
        Any a_process_writes_its_output_then_ends,
        Any stdin_written_comes_back_until_it_is_closed,
        Any a_signal_a_timeout_or_a_removal_ends_a_process,
        Any processes_in_a_sandbox_run_side_by_side,
        Any a_log_read_again_starts_after_the_entry_it_names,
        Any a_command_that_cannot_start_makes_no_job,
        Any removing_a_sandbox_ends_its_jobs_and_the_logs_read,
        Spawned closing_the_client_ends_its_server,
        Test a_push_ends_with_the_digest_it_sent_and_shows_no_credentials,
        Test a_cancelled_image_job_changes_no_tag,
        Test a_log_past_the_limit_is_truncated_but_keeps_its_end,
        Test a_mount_shows_its_type_and_not_its_credentials,
    ];

    let prune = std::env::var("VIRTX_E2E_ALLOW_PRUNE").is_ok_and(|v| v == "1");
    let (mut passed, mut ignored, mut failed) = (0, 0, Vec::new());
    for (name, needs, scenario) in scenarios {
        if !filters.is_empty() && !filters.iter().any(|f| name.contains(f.as_str())) {
            continue;
        }
        let runs = match needs {
            Needs::Any => true,
            Needs::Test => server.is_test(),
            Needs::Spawned => server.is_spawned(),
            Needs::Prune => server.is_test() || prune,
        };
        if !runs {
            eprintln!("test {name} ... ignored");
            ignored += 1;
            continue;
        }
        let run = Run {
            server: server.clone(),
            made: Arc::default(),
        };
        let ran = panic::catch_unwind(AssertUnwindSafe(|| {
            runtime.block_on(async { tokio::time::timeout(limit, scenario(run.clone())).await })
        }));
        // The test server's state goes with its process; another server keeps what it is left.
        if !server.is_test() {
            let cleaned = panic::catch_unwind(AssertUnwindSafe(|| {
                runtime.block_on(async {
                    let client = run.client().await;
                    tokio::time::timeout(limit, run.clean(&client)).await.ok();
                    client.close().await;
                })
            }));
            if cleaned.is_err() {
                eprintln!("test {name}: what it made could not all be removed");
            }
        }
        match ran {
            Ok(Ok(())) => {
                eprintln!("test {name} ... ok");
                passed += 1;
            }
            Ok(Err(_)) => {
                eprintln!("test {name} ... FAILED: still running after {limit:?}");
                failed.push(name);
            }
            Err(_) => {
                eprintln!("test {name} ... FAILED");
                failed.push(name);
            }
        }
    }

    let result = if failed.is_empty() { "ok" } else { "FAILED" };
    eprintln!(
        "\ntest result: {result}. {passed} passed; {} failed; {ignored} ignored",
        failed.len()
    );
    if failed.is_empty() {
        ExitCode::SUCCESS
    } else {
        for name in &failed {
            eprintln!("    {name}");
        }
        ExitCode::FAILURE
    }
}
