//! The tests that boot machines: sandboxes over `virtx-uvm` itself, with no server in front.
//!
//! A machine is a process its binary spawns of itself (see `machine/`), so this test has a
//! `main` of its own: spawned with `vm`, it becomes the machine; otherwise it runs each test.
//! `cargo xtask test -p virtx-uvm` signs it and puts the guest kernel beside it, which a
//! machine needs. The images are pulled once, into a store every test shares; each test keeps
//! its snapshots and volumes under a home of its own.

use std::{
    env,
    ffi::OsString,
    panic::{self, AssertUnwindSafe},
    path::{Path, PathBuf},
    process::ExitCode,
    sync::Arc,
    time::Duration,
};

use futures_util::{StreamExt as _, future::join_all};
use http::HeaderMap;
use virtx_core::{SandboxCreateArgs, SandboxJobEntry, SandboxVolume};
use virtx_uvm::{
    Files, FsTree, ImageStore, RegistryClient, Sandbox, SandboxError, SnapshotStore, VolumeError,
    VolumeStore,
};

/// What this binary is spawned with to be a machine.
const MACHINE: &str = "vm";

/// The image every sandbox here is made of.
const ALPINE: &str = "alpine:3.20";

/// A snapshot's size here: a ceiling, the file sparse.
const GIB: u64 = 1 << 30;

/// The stores a test makes its sandboxes of: the images every test shares, and snapshots and
/// volumes under a home of its own, removed when this drops.
struct Home {
    root: PathBuf,
    images: ImageStore,
    snapshots: SnapshotStore,
    volumes: VolumeStore,
}

impl Home {
    fn new(images: &ImageStore, scratch: &Path, name: &str) -> Home {
        let root = scratch.join(name);
        std::fs::create_dir_all(&root).unwrap();
        Home {
            images: images.clone(),
            snapshots: SnapshotStore::new(&root).unwrap(),
            volumes: VolumeStore::new(&root).unwrap(),
            root,
        }
    }

    /// A sandbox as `args` asks, its machine stopped once it has been idle for `park_after`;
    /// no other sandbox is taken to hold a snapshot.
    async fn sandbox(
        &self,
        args: &SandboxCreateArgs,
        park_after: Option<Duration>,
    ) -> Arc<Sandbox> {
        let vm = vec![env::current_exe().unwrap().into(), OsString::from(MACHINE)];
        let sandbox = Sandbox::create(
            &self.images,
            &self.snapshots,
            &self.volumes,
            |_| false,
            args,
            vm,
            park_after,
        );
        sandbox.await.unwrap()
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// What `cmd` wrote in `sandbox`, a line at a time, stdout and stderr alike, and its exit code;
/// or why it did not start.
async fn run(
    sandbox: &Arc<Sandbox>,
    cmd: &[&str],
    timeout: Option<Duration>,
) -> Result<(Vec<String>, i32), SandboxError> {
    let cmd = cmd.iter().map(|arg| arg.to_string()).collect();
    let job = sandbox.exec(cmd, timeout).await?;
    let mut events = std::pin::pin!(Arc::clone(job.log()).events(&HeaderMap::new()));
    let mut lines = Vec::new();
    while let Some(Ok(event)) = events.next().await {
        let event = String::from_utf8(event.to_vec()).unwrap();
        let data = event.lines().find_map(|line| line.strip_prefix("data: "));
        match serde_json::from_str(data.expect("an event carries its entry")).unwrap() {
            SandboxJobEntry::Stdout(line) | SandboxJobEntry::Stderr(line) => lines.push(line),
            SandboxJobEntry::End(end) => return Ok((lines, end.code)),
            SandboxJobEntry::Truncated {} => {}
        }
    }
    panic!("a job's log ends with its end")
}

/// Close `sandbox` as a server removes one: it blocks while the guest syncs.
async fn close(sandbox: Arc<Sandbox>) {
    tokio::task::spawn_blocking(move || sandbox.close())
        .await
        .unwrap();
}

async fn a_job_writes_its_output_and_ends_with_its_code_or_its_timeout(home: Home) {
    let sandbox = home.sandbox(&SandboxCreateArgs::new(ALPINE), None).await;
    let (mut lines, code) = run(
        &sandbox,
        &["sh", "-c", "echo one; echo two >&2; exit 3"],
        None,
    )
    .await
    .unwrap();
    // stdout and stderr are read apart, so one's lines may come before the other's.
    lines.sort();
    assert_eq!((lines, code), (vec!["one".into(), "two".into()], 3));
    let started = run(&sandbox, &["no-such-program"], None).await;
    assert!(
        matches!(started, Err(SandboxError::NotStarted(_))),
        "{started:?}"
    );
    let timed = run(&sandbox, &["sleep", "10"], Some(Duration::from_millis(200))).await;
    assert_eq!(timed.unwrap().1, 137);
    // A line past what a frame carries comes in pieces, and the machine goes on.
    let long = ["sh", "-c", "head -c 20000000 /dev/zero | tr '\\0' '\\1'"];
    let (lines, code) = run(&sandbox, &long, None).await.unwrap();
    assert_eq!((lines.concat().len(), code), (20_000_000, 0));
    close(sandbox).await;
}

async fn a_removed_job_is_killed_and_its_sandbox_kept(home: Home) {
    let sandbox = home.sandbox(&SandboxCreateArgs::new(ALPINE), None).await;
    let cmd = ["sleep", "30"].map(String::from).to_vec();
    let job = sandbox.exec(cmd, None).await.unwrap();
    assert!(job.get().running);
    sandbox.remove_job(&job.get().id).unwrap();
    assert!(sandbox.job(&job.get().id).is_err());
    assert!(sandbox.jobs().is_empty());
    tokio::time::sleep(Duration::from_millis(500)).await;
    let (lines, _) = run(&sandbox, &["sh", "-c", "ps | grep -c '[s]leep 30'"], None)
        .await
        .unwrap();
    assert_eq!(lines, ["0"]);
    close(sandbox).await;
}

async fn an_idle_sandbox_s_machine_stops_and_boots_again_with_what_it_wrote(home: Home) {
    let park = Some(Duration::from_millis(500));
    let sandbox = home.sandbox(&SandboxCreateArgs::new(ALPINE), park).await;
    // What runs past its job ends with the machine, so seeing it gone is seeing it stopped.
    let behind = ["sh", "-c", "sleep 1000 >/dev/null 2>&1 &"];
    let still = ["sh", "-c", "cat /root/kept; ps | grep -c '[s]leep 1000'"];

    run(&sandbox, &["sh", "-c", "echo kept > /root/kept"], None)
        .await
        .unwrap();
    run(&sandbox, &behind, None).await.unwrap();
    // A job that runs keeps it up, however long.
    let held = ["sh", "-c", "sleep 2; ps | grep -c '[s]leep 1000'"];
    assert_eq!(
        run(&sandbox, &held, None).await.unwrap(),
        (vec!["1".into()], 0)
    );

    // Idle, it stops; a job boots it again, over what it wrote.
    tokio::time::sleep(Duration::from_secs(2)).await;
    let (lines, _) = run(&sandbox, &still, None).await.unwrap();
    assert_eq!(lines, ["kept", "0"]);

    // Its own snapshot goes with it.
    close(sandbox).await;
    let left = std::fs::read_dir(home.root.join("sandbox"))
        .unwrap()
        .count();
    assert_eq!(left, 0);
}

async fn a_parked_sandbox_holds_its_snapshot_against_another(home: Home) {
    home.snapshots.create("work", GIB).unwrap();
    let on_work = SandboxCreateArgs::new(ALPINE).snapshot("work");
    let park = Some(Duration::from_millis(500));
    let sandbox = home.sandbox(&on_work, park).await;
    run(&sandbox, &["sh", "-c", "echo kept > /root/kept"], None)
        .await
        .unwrap();

    // Its machine stopped, the snapshot is still the sandbox's, to a sandbox no one told of it,
    // as one over the same home in another process is.
    tokio::time::sleep(Duration::from_secs(2)).await;
    let vm = vec![env::current_exe().unwrap().into(), OsString::from(MACHINE)];
    let other = Sandbox::create(
        &home.images,
        &home.snapshots,
        &home.volumes,
        |_| false,
        &on_work,
        vm,
        None,
    );
    assert!(other.await.is_err());
    let (lines, _) = run(&sandbox, &["cat", "/root/kept"], None).await.unwrap();
    assert_eq!(lines, ["kept"]);

    // Closed, it lets it go.
    close(sandbox).await;
    let again = home.sandbox(&on_work, None).await;
    close(again).await;
}

async fn a_sandbox_closed_while_its_machine_stops_has_synced(home: Home) {
    // The machine is parked about 500 ms after its last job ends, and syncs for a hundred or
    // so: one close per delay across that, each a sandbox on a snapshot of its own.
    for delay in (450..=700).step_by(50) {
        let name = format!("work-{delay}");
        home.snapshots.create(&name, GIB).unwrap();
        let args = SandboxCreateArgs::new(ALPINE).snapshot(&name);
        let sandbox = home.sandbox(&args, Some(Duration::from_millis(500))).await;
        // Left in the guest's page cache, so stopping the machine takes the while it syncs.
        let big = ["dd", "if=/dev/zero", "of=/root/big", "bs=1M", "count=256"];
        run(&sandbox, &big, None).await.unwrap();

        tokio::time::sleep(Duration::from_millis(delay)).await;
        close(sandbox).await;
        // The next machine over the snapshot sees all it wrote.
        let next = home.sandbox(&args, None).await;
        let (lines, _) = run(&next, &["stat", "-c", "%s", "/root/big"], None)
            .await
            .unwrap();
        assert_eq!(lines, [(256 << 20).to_string()], "closed after {delay} ms");
        close(next).await;
    }
}

async fn a_snapshot_a_killed_machine_left_reads_as_its_kernel_committed(home: Home) {
    home.snapshots.create("work", GIB).unwrap();
    let args = SandboxCreateArgs::new(ALPINE).snapshot("work");
    let sandbox = home.sandbox(&args, None).await;
    // Synced, so committed, and then killed with the snapshot still mounted.
    let write = "mkdir /root/kept && for i in $(seq 100); do echo $i > /root/kept/$i; done \
                 && sync && echo synced";
    let (lines, _) = run(&sandbox, &["sh", "-c", write], None).await.unwrap();
    assert_eq!(lines, ["synced"]);
    close(sandbox).await;

    let path = home.root.join("snapshot/work.ext4");
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
    let sandbox = home.sandbox(&args, None).await;
    let (lines, _) = run(&sandbox, &["cat", "/root/kept/100"], None)
        .await
        .unwrap();
    assert_eq!(lines, ["100"]);
    close(sandbox).await;
}

async fn sandboxes_made_at_once_each_run_their_own(home: Home) {
    let args = SandboxCreateArgs::new(ALPINE);
    let made = join_all((0..3).map(|_| home.sandbox(&args, None))).await;
    // Each writes and reads at once with the others, and finds what it wrote alone.
    let writes: Vec<_> = (0..made.len())
        .map(|i| format!("echo {i} > /root/mine"))
        .collect();
    let wrote = join_all(
        made.iter()
            .zip(&writes)
            .map(async |(sandbox, write)| run(sandbox, &["sh", "-c", write], None).await),
    )
    .await;
    for wrote in wrote {
        assert_eq!(wrote.unwrap().1, 0);
    }
    let read = join_all(
        made.iter()
            .map(|sandbox| run(sandbox, &["cat", "/root/mine"], None)),
    )
    .await;
    for (i, read) in read.into_iter().enumerate() {
        assert_eq!(read.unwrap(), (vec![i.to_string()], 0));
    }
    join_all(made.into_iter().map(close)).await;
}

async fn a_sandbox_closed_twice_at_once_ends_once(home: Home) {
    let sandbox = home.sandbox(&SandboxCreateArgs::new(ALPINE), None).await;
    let cmd = ["sleep", "30"].map(String::from).to_vec();
    let job = sandbox.exec(cmd, None).await.unwrap();
    tokio::join!(close(Arc::clone(&sandbox)), close(Arc::clone(&sandbox)));
    assert!(!sandbox.answer(String::new()).running);
    assert!(!job.get().running);
    assert!(sandbox.jobs().is_empty());
    let after = run(&sandbox, &["true"], None).await;
    assert!(matches!(after, Err(SandboxError::Ended)), "{after:?}");
    let left = std::fs::read_dir(home.root.join("sandbox"))
        .unwrap()
        .count();
    assert_eq!(left, 0);
}

async fn a_volume_two_sandboxes_mount_is_shared_and_held_until_both_go(home: Home) {
    home.volumes.create("data").unwrap();
    let mounting = |readonly| SandboxCreateArgs {
        volumes: vec![SandboxVolume {
            volume: "data".into(),
            path: "/data".into(),
            readonly,
        }],
        ..SandboxCreateArgs::new(ALPINE)
    };
    let (writing, reading) = (mounting(false), mounting(true));
    let (writer, reader) = tokio::join!(home.sandbox(&writing, None), home.sandbox(&reading, None));
    let write = ["sh", "-c", "echo kept > /data/kept"];
    assert_eq!(run(&writer, &write, None).await.unwrap().1, 0);
    let read = run(&reader, &["cat", "/data/kept"], None).await;
    assert_eq!(read.unwrap(), (vec!["kept".into()], 0));
    let denied = run(&reader, &["sh", "-c", "echo no > /data/no"], None).await;
    assert_ne!(denied.unwrap().1, 0);

    let in_use = || matches!(home.volumes.remove("data"), Err(VolumeError::InUse(_)));
    assert!(in_use());
    close(writer).await;
    // The other mounts it still.
    assert!(in_use());
    close(reader).await;
    home.volumes.remove("data").unwrap();
}

async fn a_parked_sandbox_holds_its_volume_against_removal(home: Home) {
    home.volumes.create("data").unwrap();
    let mounting = SandboxCreateArgs {
        volumes: vec![SandboxVolume {
            volume: "data".into(),
            path: "/data".into(),
            readonly: false,
        }],
        ..SandboxCreateArgs::new(ALPINE)
    };
    let park = Some(Duration::from_millis(500));
    let sandbox = home.sandbox(&mounting, park).await;
    run(&sandbox, &["sh", "-c", "echo kept > /data/kept"], None)
        .await
        .unwrap();

    // Its machine stopped, the volume is still the sandbox's, and there for its next machine.
    tokio::time::sleep(Duration::from_secs(2)).await;
    let removed = home.volumes.remove("data");
    assert!(matches!(removed, Err(VolumeError::InUse(_))), "{removed:?}");
    let (lines, _) = run(&sandbox, &["cat", "/data/kept"], None).await.unwrap();
    assert_eq!(lines, ["kept"]);

    // Closed, it lets it go.
    close(sandbox).await;
    home.volumes.remove("data").unwrap();
}

/// Two conditional writes to one file in a sandbox at once: the second, sent while the first is
/// still sending its body, is not held up by it and lands, and the first, checked as it lands
/// against what the second wrote, is refused `412`. For `If-None-Match: *` to a new file, and
/// for one `If-Match` to a file there.
async fn of_two_conditional_writes_to_one_file_in_a_sandbox_one_lands(home: Home) {
    use axum::body::{Body, Bytes};
    use http::{Request, header};
    use http_body_util::BodyExt as _;
    use tokio::sync::oneshot;

    let sandbox = home.sandbox(&SandboxCreateArgs::new(ALPINE), None).await;
    let files = Arc::new(Files::new(home.snapshots.clone(), home.volumes.clone()));
    let send = |request: Request<Body>| {
        let (files, sandbox) = (Arc::clone(&files), Arc::clone(&sandbox));
        async move {
            let machine = sandbox.machine().await.unwrap();
            files
                .sandbox("s", machine, sandbox.activity(), request)
                .await
        }
    };
    let at = |path: &str| format!("/sandboxes/s/files/tmp/{path}");
    let put = Request::put(at("there")).body(Body::from("there")).unwrap();
    assert_eq!(send(put).await.status(), 201);
    let head = Request::head(at("there")).body(Body::empty()).unwrap();
    let head = send(head).await;
    let etag = head.headers()[header::ETAG].to_str().unwrap().to_owned();

    for (path, condition, value) in [
        ("new", header::IF_NONE_MATCH, "*".to_owned()),
        ("there", header::IF_MATCH, etag),
    ] {
        let put = |body: Body| {
            Request::put(at(path))
                .header(condition.clone(), &value)
                .body(body)
                .unwrap()
        };
        // The first's body: a part, then the rest once let go.
        let (sending, read) = oneshot::channel();
        let (go, let_go) = oneshot::channel::<()>();
        let slow =
            futures_util::stream::unfold((Some(sending), Some(let_go)), |state| async move {
                match state {
                    (Some(sending), let_go) => {
                        let _ = sending.send(());
                        let part = Ok::<_, std::io::Error>(Bytes::from("first, "));
                        Some((part, (None, let_go)))
                    }
                    (None, Some(let_go)) => {
                        let _ = let_go.await;
                        Some((Ok(Bytes::from("whole")), (None, None)))
                    }
                    (None, None) => None,
                }
            });
        let first = tokio::spawn(send(put(Body::from_stream(slow))));
        read.await.unwrap();
        let second = send(put(Body::from("second"))).await.status();
        assert!(
            matches!(second.as_u16(), 201 | 204),
            "{path}: the second answered {second}"
        );
        go.send(()).unwrap();
        assert_eq!(first.await.unwrap().status(), 412, "{path}");
        let get = Request::get(at(path)).body(Body::empty()).unwrap();
        let read = send(get).await.into_body().collect().await.unwrap();
        assert_eq!(read.to_bytes(), "second", "{path}");
    }
    close(sandbox).await;
}

/// Two conditional writes at once to one file of a volume a sandbox mounts, one through the
/// sandbox's files and one through the volume's: the second, sent while the first is still
/// sending its body, lands, and the first, checked as it lands against what the second wrote,
/// is refused `412`, whichever path the first took. Both paths answer the file one tag, so an
/// `If-Match` taken from either is checked as it lands, not refused before.
async fn of_two_conditional_writes_to_a_mounted_volume_s_file_one_lands(home: Home) {
    use axum::body::{Body, Bytes};
    use http::{Request, header};
    use http_body_util::BodyExt as _;
    use tokio::sync::oneshot;

    home.volumes.create("data").unwrap();
    let mounting = SandboxCreateArgs {
        volumes: vec![SandboxVolume {
            volume: "data".into(),
            path: "/data".into(),
            readonly: false,
        }],
        ..SandboxCreateArgs::new(ALPINE)
    };
    let sandbox = home.sandbox(&mounting, None).await;
    let files = Arc::new(Files::new(home.snapshots.clone(), home.volumes.clone()));
    let send = |in_sandbox: bool, request: Request<Body>| {
        let (files, sandbox) = (Arc::clone(&files), Arc::clone(&sandbox));
        async move {
            match in_sandbox {
                true => {
                    let machine = sandbox.machine().await.unwrap();
                    files
                        .sandbox("s", machine, sandbox.activity(), request)
                        .await
                }
                false => files.handle(request, &|_| false).await,
            }
        }
    };
    let at = |in_sandbox: bool, path: &str| match in_sandbox {
        true => format!("/sandboxes/s/files/data/{path}"),
        false => format!("/volumes/data/files/{path}"),
    };
    let tag = |in_sandbox: bool, path: String| {
        let head = Request::head(at(in_sandbox, &path)).body(Body::empty());
        let answer = send(in_sandbox, head.unwrap());
        async move {
            let answer = answer.await;
            answer.headers()[header::ETAG].to_str().unwrap().to_owned()
        }
    };

    for (round, first_in_sandbox) in [true, false].into_iter().enumerate() {
        let there = format!("there-{round}");
        let put = Request::put(at(false, &there)).body(Body::from("there"));
        assert_eq!(send(false, put.unwrap()).await.status(), 201);
        let etag = tag(false, there.clone()).await;
        assert_eq!(
            tag(true, there.clone()).await,
            etag,
            "{there}: one file, one tag"
        );

        for (path, condition, value) in [
            (
                format!("new-{round}"),
                header::IF_NONE_MATCH,
                "*".to_owned(),
            ),
            (there, header::IF_MATCH, etag),
        ] {
            let put = |in_sandbox: bool, body: Body| {
                Request::put(at(in_sandbox, &path))
                    .header(condition.clone(), &value)
                    .body(body)
                    .unwrap()
            };
            // The first's body: a part, then the rest once let go.
            let (sending, read) = oneshot::channel();
            let (go, let_go) = oneshot::channel::<()>();
            let slow =
                futures_util::stream::unfold((Some(sending), Some(let_go)), |state| async move {
                    match state {
                        (Some(sending), let_go) => {
                            let _ = sending.send(());
                            let part = Ok::<_, std::io::Error>(Bytes::from("first, "));
                            Some((part, (None, let_go)))
                        }
                        (None, Some(let_go)) => {
                            let _ = let_go.await;
                            Some((Ok(Bytes::from("whole")), (None, None)))
                        }
                        (None, None) => None,
                    }
                });
            let first = put(first_in_sandbox, Body::from_stream(slow));
            let first = tokio::spawn(send(first_in_sandbox, first));
            read.await.unwrap();
            let second = put(!first_in_sandbox, Body::from("second"));
            let second = send(!first_in_sandbox, second).await.status();
            let how = format!("{path}, the first through the sandbox: {first_in_sandbox}");
            assert!(
                matches!(second.as_u16(), 201 | 204),
                "{how}: the second answered {second}"
            );
            go.send(()).unwrap();
            assert_eq!(first.await.unwrap().status(), 412, "{how}");
            let get = Request::get(at(false, &path)).body(Body::empty()).unwrap();
            let read = send(false, get).await.into_body().collect().await.unwrap();
            assert_eq!(read.to_bytes(), "second", "{how}");
        }
    }

    // A conditional write through the sandbox lands only once it holds the volume too: while
    // its `writing` file is held from here, as another server over the home would hold it, the
    // write neither lands nor is answered; let go, it lands.
    let writing = home.volumes.open("data").unwrap().path().join("writing");
    let writing = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(writing)
        .unwrap();
    writing.lock().unwrap();
    let put = Request::put(at(true, "held"))
        .header(header::IF_NONE_MATCH, "*")
        .body(Body::from("held"))
        .unwrap();
    let held = tokio::spawn(send(true, put));
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        !held.is_finished(),
        "it was answered while the volume was held"
    );
    let get = Request::head(at(false, "held"))
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        send(false, get).await.status(),
        404,
        "it landed while the volume was held"
    );
    writing.unlock().unwrap();
    assert_eq!(held.await.unwrap().status(), 201);
    close(sandbox).await;
}

/// The test name `args` filter by, as libtest takes one: the first that is neither an option nor
/// the value of one -- `cargo xtask test -- --include-ignored --test-threads 1` filters by
/// nothing, not by `1`.
fn filter(mut args: impl Iterator<Item = String>) -> Option<String> {
    const VALUED: [&str; 7] = [
        "--test-threads",
        "--skip",
        "--color",
        "--format",
        "--logfile",
        "--shuffle-seed",
        "-Z",
    ];
    while let Some(arg) = args.next() {
        if VALUED.contains(&arg.as_str()) {
            args.next();
        } else if !arg.starts_with('-') {
            return Some(arg);
        }
    }
    None
}

fn main() -> ExitCode {
    // Before any runtime: this process is about to become a machine and never come back.
    if env::args_os().nth(1).is_some_and(|arg| arg == MACHINE) {
        let Err(_) = virtx_uvm::enter_machine();
        std::process::exit(1);
    }

    let scratch = env::temp_dir().join(format!("virtx-uvm-machine-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let images = ImageStore::new(scratch.join("images")).unwrap();
    runtime.block_on(async {
        let reference = ALPINE.parse().unwrap();
        let registry = RegistryClient::new(images.clone(), None);
        let (_, _, manifest) = registry.pull_manifest(&reference).await.unwrap();
        let (_, config) = registry.pull_config(&reference, &manifest).await.unwrap();
        let mut layers = Vec::new();
        for descriptor in manifest.layers() {
            let tree = registry.pull_layer(&reference, descriptor).await.unwrap();
            layers.push(images.insert_layer(&tree).unwrap());
        }
        let image = images.insert_image(&config, &layers).unwrap();
        images.insert_tag(&reference, &image).unwrap();
    });

    type Test = fn(Home) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()>>>;
    macro_rules! tests {
        ($($test:ident),* $(,)?) => {
            [$((stringify!($test), (|home| Box::pin($test(home))) as Test)),*]
        };
    }
    let tests = tests![
        a_job_writes_its_output_and_ends_with_its_code_or_its_timeout,
        a_removed_job_is_killed_and_its_sandbox_kept,
        an_idle_sandbox_s_machine_stops_and_boots_again_with_what_it_wrote,
        a_parked_sandbox_holds_its_snapshot_against_another,
        a_sandbox_closed_while_its_machine_stops_has_synced,
        a_snapshot_a_killed_machine_left_reads_as_its_kernel_committed,
        sandboxes_made_at_once_each_run_their_own,
        a_sandbox_closed_twice_at_once_ends_once,
        a_volume_two_sandboxes_mount_is_shared_and_held_until_both_go,
        a_parked_sandbox_holds_its_volume_against_removal,
        of_two_conditional_writes_to_one_file_in_a_sandbox_one_lands,
        of_two_conditional_writes_to_a_mounted_volume_s_file_one_lands,
    ];
    let filter = filter(env::args().skip(1));
    let mut failed = 0;
    for (name, test) in tests {
        if filter
            .as_deref()
            .is_some_and(|filter| !name.contains(filter))
        {
            continue;
        }
        let home = Home::new(&images, &scratch, name);
        let passed = panic::catch_unwind(AssertUnwindSafe(|| runtime.block_on(test(home)))).is_ok();
        println!("test {name} ... {}", if passed { "ok" } else { "FAILED" });
        failed += usize::from(!passed);
    }
    let _ = std::fs::remove_dir_all(&scratch);
    if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
