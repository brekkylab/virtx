//! The tests that boot machines: sandboxes over `virtx-uvm` itself, with no server in front.
//!
//! A machine is a process its binary spawns of itself (see `machine/`), so this test has a
//! `main` of its own: spawned with `vm`, it becomes the machine; otherwise it runs each test.
//! `cargo xtask test -p virtx-uvm` signs it and puts the guest kernel beside it, which a
//! machine needs. The images are pulled once, into a store every test shares; each test keeps
//! its sandboxes' disks and its volumes under a home of its own.

use std::{
    env,
    ffi::OsString,
    panic::{self, AssertUnwindSafe},
    path::{Path, PathBuf},
    process::ExitCode,
    sync::Arc,
    time::{Duration, Instant},
};

use futures_util::{StreamExt as _, future::join_all};
use http::HeaderMap;
use virtx_core::{SandboxCreateArgs, SandboxJobEntry, SandboxVolume};
use virtx_uvm::sandbox::{NoHooks, Sandboxes, set_minute};
use virtx_uvm::{
    FsTree, ImageStore, RegistryClient, Sandbox, SandboxError, SnapshotStore, VolumeError,
    VolumeStore,
};

/// What this binary is spawned with to be a machine.
const MACHINE: &str = "vm";

/// The image every sandbox here is made of, and where it is pulled from: Docker Hub's official
/// images from Google's mirror of them, as Docker Hub refuses anonymous pulls past a few an
/// hour.
const ALPINE: &str = "alpine:3.20";
const ALPINE_FROM: &str = "mirror.gcr.io/library/alpine:3.20";

/// A minute, as a sandbox's `auto_stop_minutes` counts it here: what stops one unused is
/// tried in moments, not minutes.
const MINUTE: Duration = Duration::from_millis(100);

/// How long a test looks for what it waits on before it fails: a guard against a hang, not a
/// measure of how soon.
const HANG: Duration = Duration::from_secs(60);

/// The stores a test makes its sandboxes of: the images every test shares, and the disks and
/// volumes under a home of its own, removed when this drops.
struct Home {
    root: PathBuf,
    volumes: VolumeStore,
    sandboxes: Sandboxes,
}

impl Home {
    fn new(images: &ImageStore, scratch: &Path, name: &str) -> Home {
        let root = scratch.join(name);
        std::fs::create_dir_all(&root).unwrap();
        let volumes = VolumeStore::new(&root).unwrap();
        Home {
            sandboxes: sandboxes(images, &root),
            volumes,
            root,
        }
    }

    /// A sandbox as `args` asks, booted.
    async fn sandbox(&self, args: &SandboxCreateArgs) -> Arc<Sandbox> {
        let made = self.sandboxes.create(args.clone(), None, true).await;
        self.sandboxes.get(&made.unwrap().id).unwrap()
    }

    /// Where `sandbox`'s disk is.
    fn disk(&self, sandbox: &Sandbox) -> PathBuf {
        self.root.join(format!("snapshot/{}.ext4", sandbox.id()))
    }

    /// What is kept of the sandboxes here: a record and a disk each.
    fn kept(&self) -> usize {
        let count = |dir: &str| {
            std::fs::read_dir(self.root.join(dir))
                .map(|entries| entries.count())
                .unwrap_or(0)
        };
        count("sandbox") + count("snapshot")
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// The sandboxes kept under `root`, as a server over it has them.
fn sandboxes(images: &ImageStore, root: &Path) -> Sandboxes {
    let vm = vec![env::current_exe().unwrap().into(), OsString::from(MACHINE)];
    Sandboxes::new(
        images.clone(),
        SnapshotStore::new(root).unwrap(),
        VolumeStore::new(root).unwrap(),
        root,
        vm,
        Arc::new(NoHooks),
    )
    .unwrap()
}

/// What `cmd` wrote in `sandbox`, a line at a time, stdout and stderr alike, and its exit code;
/// or why it did not start.
async fn run(
    sandbox: &Arc<Sandbox>,
    cmd: &[&str],
    timeout: Option<Duration>,
) -> Result<(Vec<String>, i32), SandboxError> {
    let cmd = cmd.iter().map(|arg| arg.to_string()).collect();
    let job = sandbox.exec(cmd, false, timeout).await?;
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

/// Looked for until `holds` does, for as long as [`HANG`].
async fn until(mut holds: impl AsyncFnMut() -> bool) -> bool {
    let began = Instant::now();
    while began.elapsed() < HANG {
        if holds().await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

async fn a_job_writes_its_output_and_ends_with_its_code_or_its_timeout(home: Home) {
    let sandbox = home.sandbox(&SandboxCreateArgs::new(ALPINE)).await;
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
    let timed = run(
        &sandbox,
        &["sleep", "300"],
        Some(Duration::from_millis(200)),
    )
    .await;
    assert_eq!(timed.unwrap().1, 137);
    // A line past what a frame carries comes in pieces, and the machine goes on.
    let long = ["sh", "-c", "head -c 20000000 /dev/zero | tr '\\0' '\\1'"];
    let (lines, code) = run(&sandbox, &long, None).await.unwrap();
    assert_eq!((lines.concat().len(), code), (20_000_000, 0));
    home.sandboxes.delete(sandbox.id()).await.unwrap();
}

async fn a_removed_job_is_killed_and_its_sandbox_kept(home: Home) {
    let sandbox = home.sandbox(&SandboxCreateArgs::new(ALPINE)).await;
    let cmd = ["sleep", "300"].map(String::from).to_vec();
    let job = sandbox.exec(cmd, false, None).await.unwrap();
    assert!(job.get().running);
    sandbox.remove_job(&job.get().id).unwrap();
    assert!(sandbox.job(&job.get().id).is_err());
    assert!(sandbox.jobs().is_empty());
    let count = ["sh", "-c", "ps | grep -c '[s]leep 300'"];
    let gone = until(async || run(&sandbox, &count, None).await.unwrap().0 == ["0"]).await;
    assert!(gone, "the removed job's process runs on");
    home.sandboxes.delete(sandbox.id()).await.unwrap();
}

async fn an_unused_sandbox_stops_and_starts_again_with_what_it_wrote(home: Home) {
    home.sandboxes.spawn_tending();
    let args = SandboxCreateArgs::new(ALPINE).auto_stop_minutes(5);
    let sandbox = home.sandbox(&args).await;
    // What runs past its job ends with the machine, so seeing it gone is seeing it stopped.
    let behind = ["sh", "-c", "sleep 1000 >/dev/null 2>&1 &"];
    let still = ["sh", "-c", "cat /root/kept; ps | grep -c '[s]leep 1000'"];

    run(&sandbox, &["sh", "-c", "echo kept > /root/kept"], None)
        .await
        .unwrap();
    run(&sandbox, &behind, None).await.unwrap();
    // A job that runs keeps it up, past its auto-stop many times over.
    let held = ["sh", "-c", "sleep 3; ps | grep -c '[s]leep 1000'"];
    assert_eq!(
        run(&sandbox, &held, None).await.unwrap(),
        (vec!["1".into()], 0)
    );

    // Unused, it stops; a job starts it again, over what it wrote.
    assert!(until(async || !sandbox.is_running()).await, "not stopped");
    let (lines, _) = run(&sandbox, &still, None).await.unwrap();
    assert_eq!(lines, ["kept", "0"]);

    // Deleted, its disk goes with it.
    home.sandboxes.delete(sandbox.id()).await.unwrap();
    assert_eq!(home.kept(), 0);
}

async fn a_running_sandbox_s_disk_is_held_against_another_server(home: Home) {
    let sandbox = home.sandbox(&SandboxCreateArgs::new(ALPINE)).await;
    run(&sandbox, &["sh", "-c", "echo kept > /root/kept"], None)
        .await
        .unwrap();

    // Another server over the same home knows it, stopped, and cannot start it while its disk
    // is this machine's.
    let other = sandboxes(home.sandboxes.images(), &home.root);
    let there = other.get(sandbox.id()).unwrap();
    assert!(!there.is_running());
    let refused = there.start().await;
    assert!(
        matches!(refused, Err(SandboxError::Snapshot(_))),
        "{refused:?}"
    );
    let (lines, _) = run(&sandbox, &["cat", "/root/kept"], None).await.unwrap();
    assert_eq!(lines, ["kept"]);

    // Stopped here, it is the other's to start, as it was left.
    home.sandboxes.stop(sandbox.id()).await.unwrap();
    let (lines, _) = run(&there, &["cat", "/root/kept"], None).await.unwrap();
    assert_eq!(lines, ["kept"]);
    other.delete(sandbox.id()).await.unwrap();
}

async fn a_sandbox_stopped_while_it_stops_unused_has_synced(home: Home) {
    home.sandboxes.spawn_tending();
    // It stops about 500 ms after its last job ends, and syncs for a hundred or so: one stop
    // asked for at each delay across that, each a sandbox of its own.
    for delay in (450..=700).step_by(50) {
        let args = SandboxCreateArgs::new(ALPINE).auto_stop_minutes(5);
        let sandbox = home.sandbox(&args).await;
        // Left in the guest's page cache, so stopping the machine takes the while it syncs.
        let big = ["dd", "if=/dev/zero", "of=/root/big", "bs=1M", "count=256"];
        run(&sandbox, &big, None).await.unwrap();

        tokio::time::sleep(Duration::from_millis(delay)).await;
        home.sandboxes.stop(sandbox.id()).await.unwrap();
        // Started again, it has all it wrote.
        let (lines, _) = run(&sandbox, &["stat", "-c", "%s", "/root/big"], None)
            .await
            .unwrap();
        assert_eq!(lines, [(256 << 20).to_string()], "stopped after {delay} ms");
        home.sandboxes.delete(sandbox.id()).await.unwrap();
    }
}

async fn a_disk_a_stopped_machine_left_reads_as_its_kernel_committed(home: Home) {
    let sandbox = home.sandbox(&SandboxCreateArgs::new(ALPINE)).await;
    let write = "mkdir /root/kept && for i in $(seq 100); do echo $i > /root/kept/$i; done \
                 && sync && echo synced";
    let (lines, _) = run(&sandbox, &["sh", "-c", write], None).await.unwrap();
    assert_eq!(lines, ["synced"]);
    home.sandboxes.stop(sandbox.id()).await.unwrap();

    let path = home.disk(&sandbox);
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
    let (lines, _) = run(&sandbox, &["cat", "/root/kept/100"], None)
        .await
        .unwrap();
    assert_eq!(lines, ["100"]);
    home.sandboxes.delete(sandbox.id()).await.unwrap();
}

async fn sandboxes_made_at_once_each_run_their_own(home: Home) {
    let args = SandboxCreateArgs::new(ALPINE);
    let made = join_all((0..3).map(|_| home.sandbox(&args))).await;
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
    join_all(
        made.iter()
            .map(|sandbox| home.sandboxes.delete(sandbox.id())),
    )
    .await;
}

async fn a_sandbox_deleted_twice_at_once_ends_once(home: Home) {
    let sandbox = home.sandbox(&SandboxCreateArgs::new(ALPINE)).await;
    let cmd = ["sleep", "300"].map(String::from).to_vec();
    let job = sandbox.exec(cmd, false, None).await.unwrap();
    let (first, second) = tokio::join!(
        home.sandboxes.delete(sandbox.id()),
        home.sandboxes.delete(sandbox.id())
    );
    // One deletes it; the other finds it deleted, or gone.
    for deleted in [first, second] {
        assert!(
            matches!(deleted, Ok(()) | Err(SandboxError::NotFound(_))),
            "{deleted:?}"
        );
    }
    assert!(!sandbox.is_running());
    assert!(!job.get().running);
    assert!(sandbox.jobs().is_empty());
    let after = run(&sandbox, &["true"], None).await;
    assert!(matches!(after, Err(SandboxError::NotFound(_))), "{after:?}");
    assert_eq!(home.kept(), 0);
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
    let (writer, reader) = tokio::join!(home.sandbox(&writing), home.sandbox(&reading));
    let write = ["sh", "-c", "echo kept > /data/kept"];
    assert_eq!(run(&writer, &write, None).await.unwrap().1, 0);
    let read = run(&reader, &["cat", "/data/kept"], None).await;
    assert_eq!(read.unwrap(), (vec!["kept".into()], 0));
    let denied = run(&reader, &["sh", "-c", "echo no > /data/no"], None).await;
    assert_ne!(denied.unwrap().1, 0);

    let in_use = || matches!(home.volumes.remove("data"), Err(VolumeError::InUse(_)));
    assert!(in_use());
    home.sandboxes.delete(writer.id()).await.unwrap();
    // The other mounts it still.
    assert!(in_use());
    home.sandboxes.delete(reader.id()).await.unwrap();
    home.volumes.remove("data").unwrap();
}

fn main() -> ExitCode {
    // Before any runtime: this process is about to become a machine and never come back.
    if env::args_os().nth(1).is_some_and(|arg| arg == MACHINE) {
        let Err(_) = virtx_uvm::enter_machine();
        std::process::exit(1);
    }
    set_minute(MINUTE);

    let scratch = env::temp_dir().join(format!("virtx-uvm-machine-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let images = ImageStore::new(scratch.join("images")).unwrap();
    runtime.block_on(async {
        let from = ALPINE_FROM.parse().unwrap();
        let registry = RegistryClient::new(images.clone(), None);
        let (_, _, manifest) = registry.pull_manifest(&from).await.unwrap();
        let (_, config) = registry.pull_config(&from, &manifest).await.unwrap();
        let mut layers = Vec::new();
        for descriptor in manifest.layers() {
            let tree = registry.pull_layer(&from, descriptor).await.unwrap();
            layers.push(images.insert_layer(&tree).unwrap());
        }
        let image = images.insert_image(&config, &layers).unwrap();
        images.insert_tag(&ALPINE.parse().unwrap(), &image).unwrap();
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
        an_unused_sandbox_stops_and_starts_again_with_what_it_wrote,
        a_running_sandbox_s_disk_is_held_against_another_server,
        a_sandbox_stopped_while_it_stops_unused_has_synced,
        a_disk_a_stopped_machine_left_reads_as_its_kernel_committed,
        sandboxes_made_at_once_each_run_their_own,
        a_sandbox_deleted_twice_at_once_ends_once,
        a_volume_two_sandboxes_mount_is_shared_and_held_until_both_go,
    ];
    // libtest's flags, which a run over every crate passes, are passed over: `--test-threads`
    // with its value.
    let args: Vec<String> = env::args().skip(1).collect();
    let filter = args
        .iter()
        .enumerate()
        .find(|(i, arg)| !arg.starts_with('-') && (*i == 0 || args[i - 1] != "--test-threads"))
        .map(|(_, arg)| arg.clone());
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
