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
use virtx_core::{SandboxCreateArgs, SandboxJobEndReason, SandboxJobEntry, SandboxVolume};
use virtx_uvm::sandbox::{NoHooks, Sandboxes};
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

/// The image a GPU sandbox is made of, and where it is pulled from: edge, the one Alpine whose
/// repositories carry venus's guest driver (`mesa-vulkan-virtio`); no stable branch does yet.
/// Pulled only for the GPU's tests.
const GPU_IMAGE: &str = "alpine:edge";
const GPU_IMAGE_FROM: &str = "mirror.gcr.io/library/alpine:edge";

/// A sandbox of [`ALPINE`] with 1 GiB of memory: enough for what these tests run in it, and little
/// enough that the suite's machines, booting at once, fit a host that commits each one's memory as
/// it boots (`VIRTX_PREFAULT_MEMORY`, as Windows' CI runs them), where 4 GiB each does not.
fn small() -> SandboxCreateArgs {
    let mut args = SandboxCreateArgs::new(ALPINE);
    args.memory_mib = Some(1024);
    args
}

/// How long a sandbox is unused before it is parked, in a test that parks one so: moments, not
/// a minute.
const PARK_AFTER: Duration = Duration::from_millis(500);

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
            sandboxes: sandboxes(images, &root, None),
            volumes,
            root,
        }
    }

    /// It, its sandboxes parked once unused for [`PARK_AFTER`]: before any is made.
    fn parking(mut self) -> Home {
        self.sandboxes = sandboxes(self.sandboxes.images(), &self.root, Some(PARK_AFTER));
        self
    }

    /// A sandbox as `args` asks, booted.
    async fn sandbox(&self, args: &SandboxCreateArgs) -> Arc<Sandbox> {
        let made = self.sandboxes.create(args.clone(), None, true).await;
        self.sandboxes.get(&made.unwrap().id).unwrap()
    }

    /// Where `sandbox`'s disk is.
    fn disk(&self, sandbox: &Sandbox) -> PathBuf {
        self.root.join(format!("sandbox/{}.ext4", sandbox.id()))
    }

    /// What is kept of the sandboxes here: a record and a disk each.
    fn kept(&self) -> usize {
        std::fs::read_dir(self.root.join("sandbox"))
            .map(|entries| {
                entries
                    .flatten()
                    .filter(|entry| {
                        let name = entry.file_name();
                        let name = name.to_string_lossy();
                        name.ends_with(".json") || name.ends_with(".ext4")
                    })
                    .count()
            })
            .unwrap_or(0)
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// The sandboxes kept under `root`, as a server over it has them, each parked once unused for
/// `park_after`.
fn sandboxes(images: &ImageStore, root: &Path, park_after: Option<Duration>) -> Sandboxes {
    let vm = vec![env::current_exe().unwrap().into(), OsString::from(MACHINE)];
    Sandboxes::new(
        images.clone(),
        SnapshotStore::new(root).unwrap(),
        VolumeStore::new(root).unwrap(),
        root,
        vm,
        Arc::new(NoHooks),
        park_after,
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
    let sandbox = home.sandbox(&small()).await;
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
    let jobs = sandbox.jobs();
    let timed = jobs.iter().find(|job| job.cmd == ["sleep", "300"]).unwrap();
    assert_eq!(timed.reason, Some(SandboxJobEndReason::Timeout));
    assert!(jobs.iter().all(|job| job == timed || job.reason.is_none()));
    // A line past what a frame carries comes in pieces, and the machine goes on.
    let long = ["sh", "-c", "head -c 20000000 /dev/zero | tr '\\0' '\\1'"];
    let (lines, code) = run(&sandbox, &long, None).await.unwrap();
    assert_eq!((lines.concat().len(), code), (20_000_000, 0));
    home.sandboxes.delete(sandbox.id()).await.unwrap();
}

async fn a_removed_job_is_killed_and_its_sandbox_kept(home: Home) {
    let sandbox = home.sandbox(&small()).await;
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

async fn a_job_ends_with_its_process_not_with_what_it_left_running(home: Home) {
    let sandbox = home.sandbox(&small()).await;
    // A program left running with the job's own output: the job ends with its shell, and the
    // program runs on.
    let left = ["sh", "-c", "sleep 300 & echo started; printf 'no newline'"];
    let ended = tokio::time::timeout(HANG, run(&sandbox, &left, None));
    let (lines, code) = ended.await.expect("the job ends with its shell").unwrap();
    assert_eq!(
        (lines, code),
        (vec!["started".into(), "no newline".into()], 0)
    );
    let still = [
        "sh",
        "-c",
        "pgrep -f 'sleep 300' >/dev/null && echo running",
    ];
    assert_eq!(run(&sandbox, &still, None).await.unwrap().0, ["running"]);

    // One that writes on: what the job wrote is its log, whole, and what is left running is
    // never held up by a pipe nothing reads any more. It writes to stderr in lines of x.
    let writer = "(while true; do head -c 8000 /dev/zero | tr '\\0' x; echo; date >> /tmp/w; \
                  sleep 0.01; done) >&2 & seq 1 20000; echo done";
    let writes = ["sh", "-c", writer];
    let ended = tokio::time::timeout(HANG, run(&sandbox, &writes, None));
    let (lines, code) = ended.await.expect("the job ends with its shell").unwrap();
    // Its own lines are seq's and `done`; the rest are the writer's, of x alone.
    let own: Vec<&str> = lines
        .iter()
        .map(String::as_str)
        .filter(|line| line.is_empty() || line.bytes().any(|b| b != b'x'))
        .collect();
    let seq: Vec<String> = (1..=20_000).map(|n| n.to_string()).collect();
    assert!(
        own.len() == 20_001 && own[..20_000] == seq && own[20_000] == "done",
        "{} lines, not seq's: {:?}",
        own.len(),
        own.iter()
            .filter(|l| l.parse::<u32>().is_err())
            .collect::<Vec<_>>()
    );
    assert_eq!(code, 0);
    let moving = "sleep 1; a=$(wc -l < /tmp/w); sleep 2; b=$(wc -l < /tmp/w); \
                  [ $b -gt $((a + 20)) ] && echo moving";
    let (lines, _) = run(&sandbox, &["sh", "-c", moving], None).await.unwrap();
    assert_eq!(lines, ["moving"], "what the job left running was held up");
    home.sandboxes.delete(sandbox.id()).await.unwrap();
}

async fn a_job_s_stdin_is_written_until_it_is_closed(home: Home) {
    let sandbox = home.sandbox(&small()).await;
    let cmd = ["sh", "-c", "tr a-z A-Z; echo done"]
        .map(String::from)
        .to_vec();
    let job = sandbox.exec(cmd, true, None).await.unwrap();
    let id = job.get().id;
    sandbox
        .write_stdin(&id, "hello\n".into(), false)
        .await
        .unwrap();
    sandbox
        .write_stdin(&id, "world\n".into(), true)
        .await
        .unwrap();
    let mut events = std::pin::pin!(Arc::clone(job.log()).events(&HeaderMap::new()));
    let mut lines = Vec::new();
    while let Some(Ok(event)) = events.next().await {
        let event = String::from_utf8(event.to_vec()).unwrap();
        let data = event
            .lines()
            .find_map(|line| line.strip_prefix("data: "))
            .unwrap();
        if let SandboxJobEntry::Stdout(line) = serde_json::from_str(data).unwrap() {
            lines.push(line);
        }
    }
    assert_eq!(lines, ["HELLO", "WORLD", "done"]);
    // Closed, and ended: refused.
    let refused = sandbox.write_stdin(&id, "x".into(), false).await;
    assert!(
        matches!(refused, Err(SandboxError::JobEnded(_))),
        "{refused:?}"
    );

    // Started without one: its stdin is empty, and a write is refused.
    let cmd = ["sh", "-c", "sleep 1; cat; echo eof"]
        .map(String::from)
        .to_vec();
    let job = sandbox.exec(cmd, false, None).await.unwrap();
    let refused = sandbox.write_stdin(&job.get().id, "x".into(), false).await;
    assert!(
        matches!(refused, Err(SandboxError::JobEnded(_))),
        "{refused:?}"
    );
    let (lines, code) = run(&sandbox, &["sh", "-c", "cat; echo eof"], None)
        .await
        .unwrap();
    assert_eq!((lines, code), (vec!["eof".into()], 0));
    home.sandboxes.delete(sandbox.id()).await.unwrap();
}

async fn stdin_a_job_does_not_read_holds_back_its_writer_and_nothing_else(home: Home) {
    let sandbox = home.sandbox(&small()).await;
    let cmd = ["sleep", "600"].map(String::from).to_vec();
    let sleeper = sandbox.exec(cmd, true, None).await.unwrap();
    let id = sleeper.get().id;
    // Far past what its pipe holds: not written while nothing reads it.
    let writing = {
        let (sandbox, id) = (Arc::clone(&sandbox), id.clone());
        tokio::spawn(async move { sandbox.write_stdin(&id, "x".repeat(1 << 20), false).await })
    };
    // A command beside it is answered meanwhile.
    let beside = tokio::time::timeout(HANG, run(&sandbox, &["echo", "beside"], None));
    let (lines, _) = beside.await.expect("answered beside").unwrap();
    assert_eq!(lines, ["beside"]);
    assert!(
        !writing.is_finished(),
        "written to a process that does not read"
    );
    // Once it ends, so does the write, as ended.
    sandbox.remove_job(&id).unwrap();
    let written = tokio::time::timeout(HANG, writing)
        .await
        .expect("the write ends");
    assert!(matches!(written.unwrap(), Err(SandboxError::JobEnded(_))));
    home.sandboxes.delete(sandbox.id()).await.unwrap();
}

async fn an_unused_sandbox_is_parked_and_booted_again_with_what_it_wrote(home: Home) {
    let home = home.parking();
    let sandbox = home.sandbox(&small()).await;
    // What runs past its job ends with the machine, so seeing it gone is seeing it parked.
    let behind = ["sh", "-c", "sleep 1000 >/dev/null 2>&1 &"];
    let still = ["sh", "-c", "cat /root/kept; ps | grep -c '[s]leep 1000'"];

    run(&sandbox, &["sh", "-c", "echo kept > /root/kept"], None)
        .await
        .unwrap();
    run(&sandbox, &behind, None).await.unwrap();
    // A job that runs keeps it up, past its park many times over.
    let held = ["sh", "-c", "sleep 3; ps | grep -c '[s]leep 1000'"];
    assert_eq!(
        run(&sandbox, &held, None).await.unwrap(),
        (vec!["1".into()], 0)
    );

    // Unused, it is parked, and running still; a job boots it again, over what it wrote.
    assert!(until(async || !sandbox.up()).await, "not parked");
    assert!(sandbox.answer(false).running);
    let (lines, _) = run(&sandbox, &still, None).await.unwrap();
    assert_eq!(lines, ["kept", "0"]);

    // Parked again, its machine asked for, as a write to its files asks for it, boots it too.
    assert!(until(async || !sandbox.up()).await, "not parked");
    let _asking = sandbox.activity();
    sandbox.machine().await.unwrap();
    assert!(sandbox.up());
    drop(_asking);

    // In use, it is not parked when asked to; parking every sandbox parks it all the same,
    // and ends its job.
    let cmd = ["sleep", "300"].map(String::from).to_vec();
    let job = sandbox.exec(cmd, false, None).await.unwrap();
    let refused = sandbox.park().await;
    assert!(
        matches!(refused, Err(SandboxError::InUse(_))),
        "{refused:?}"
    );
    assert!(sandbox.up());
    home.sandboxes.park_all().await;
    assert!(!sandbox.up());
    let ended = job.get();
    assert_eq!(
        (ended.code, ended.reason),
        (Some(137), Some(SandboxJobEndReason::Cancelled))
    );

    // Deleted, its disk goes with it.
    home.sandboxes.delete(sandbox.id()).await.unwrap();
    assert_eq!(home.kept(), 0);
}

async fn a_sandbox_whose_machine_ends_on_its_own_has_exited_for_good(home: Home) {
    let sandbox = home.sandbox(&small()).await;
    // Reset, which ends a machine on every arch and every guest kernel.
    let ended = tokio::time::timeout(HANG, run(&sandbox, &["sh", "-c", "sync; reboot -f"], None));
    let (_, code) = ended.await.expect("its machine ended").unwrap();
    assert_eq!(code, 137, "ended with its machine");
    let reasons: Vec<_> = sandbox.jobs().iter().map(|job| job.reason).collect();
    assert_eq!(reasons, [Some(SandboxJobEndReason::VmExited)]);
    assert!(until(async || !sandbox.answer(false).running).await);
    let refused = run(&sandbox, &["true"], None).await;
    assert!(matches!(refused, Err(SandboxError::Ended)), "{refused:?}");

    // A server started again over the same home knows it exited.
    let other = sandboxes(home.sandboxes.images(), &home.root, None);
    let there = other.get(sandbox.id()).unwrap();
    assert!(!there.answer(false).running);
    let refused = there.start().await;
    assert!(matches!(refused, Err(SandboxError::Ended)), "{refused:?}");
    home.sandboxes.delete(sandbox.id()).await.unwrap();
}

async fn a_running_sandbox_s_disk_is_held_against_another_server(home: Home) {
    let sandbox = home.sandbox(&small()).await;
    run(&sandbox, &["sh", "-c", "echo kept > /root/kept"], None)
        .await
        .unwrap();

    // Another server over the same home knows it, parked, and cannot boot it while its disk
    // is this machine's.
    let other = sandboxes(home.sandboxes.images(), &home.root, None);
    let there = other.get(sandbox.id()).unwrap();
    assert!(!there.up());
    let refused = there.start().await;
    assert!(
        matches!(refused, Err(SandboxError::Snapshot(_))),
        "{refused:?}"
    );
    let (lines, _) = run(&sandbox, &["cat", "/root/kept"], None).await.unwrap();
    assert_eq!(lines, ["kept"]);

    // Parked here, it is the other's to boot, as it was left.
    sandbox.park().await.unwrap();
    let (lines, _) = run(&there, &["cat", "/root/kept"], None).await.unwrap();
    assert_eq!(lines, ["kept"]);
    other.delete(sandbox.id()).await.unwrap();
}

async fn a_guest_that_powers_itself_off_ends_its_machine_and_its_jobs(home: Home) {
    let sandbox = home.sandbox(&small()).await;
    let off = ["sh", "-c", "sleep 1; poweroff -f"];
    let ended = tokio::time::timeout(HANG, run(&sandbox, &off, None));
    // Its job never says it ended: it ends as killed, with the machine.
    let (_, code) = ended.await.expect("its machine ended").unwrap();
    assert_eq!(code, 137, "ended with its machine");
    let reasons: Vec<_> = sandbox.jobs().iter().map(|job| job.reason).collect();
    assert_eq!(reasons, [Some(SandboxJobEndReason::VmExited)]);
    assert!(until(async || !sandbox.answer(false).running).await);
    let refused = run(&sandbox, &["true"], None).await;
    assert!(matches!(refused, Err(SandboxError::Ended)), "{refused:?}");
    home.sandboxes.delete(sandbox.id()).await.unwrap();
}

async fn a_sandbox_parked_as_it_is_parked_unused_has_synced(home: Home) {
    let home = home.parking();
    // It is parked about 500 ms after its last job ends, and syncs for a hundred or so: one
    // park asked for at each delay across that, each a sandbox of its own.
    for delay in (450..=700).step_by(50) {
        let sandbox = home.sandbox(&small()).await;
        // Left in the guest's page cache, so stopping the machine takes the while it syncs.
        let big = ["dd", "if=/dev/zero", "of=/root/big", "bs=1M", "count=256"];
        run(&sandbox, &big, None).await.unwrap();

        tokio::time::sleep(Duration::from_millis(delay)).await;
        sandbox.park().await.unwrap();
        // Booted again, it has all it wrote.
        let (lines, _) = run(&sandbox, &["stat", "-c", "%s", "/root/big"], None)
            .await
            .unwrap();
        assert_eq!(lines, [(256 << 20).to_string()], "parked after {delay} ms");
        home.sandboxes.delete(sandbox.id()).await.unwrap();
    }
}

async fn a_disk_a_parked_machine_left_reads_as_its_kernel_committed(home: Home) {
    let sandbox = home.sandbox(&small()).await;
    let write = "mkdir /root/kept && for i in $(seq 100); do echo $i > /root/kept/$i; done \
                 && sync && echo synced";
    let (lines, _) = run(&sandbox, &["sh", "-c", write], None).await.unwrap();
    assert_eq!(lines, ["synced"]);
    sandbox.park().await.unwrap();

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
    let args = small();
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
    let sandbox = home.sandbox(&small()).await;
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
    assert!(!sandbox.up());
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
        ..small()
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

/// What a guest installs to reach venus and ask it what it finds: venus's guest driver, the
/// Vulkan loader, and `vulkaninfo`. The image carries none of it; a guest without it boots
/// and finds no device, which is not what the GPU's tests ask.
const GPU_GUEST: [&str; 3] = ["mesa-vulkan-virtio", "vulkan-loader", "vulkan-tools"];

/// And what it installs to build `tests/gpu/gpu.c` and its shader against them.
const GPU_BUILD: [&str; 4] = [
    "build-base",
    "vulkan-headers",
    "vulkan-loader-dev",
    "glslang",
];

/// What the GPU's tests run on the guest's GPU, and the compute shader it dispatches -- see
/// the file.
const GPU_C: &str = include_str!("gpu/gpu.c");
const TRIPLE_COMP: &str = include_str!("gpu/triple.comp");

/// The host rasterizers venus hands a guest beside a GPU -- see [`is_gpu`].
const SOFTWARE: [&str; 3] = ["llvmpipe", "lavapipe", "SwiftShader"];

/// Whether a device the guest's Vulkan names is the host's GPU through venus.
///
/// venus hands the guest every Vulkan device the host has, the host's software rasterizer
/// among them where one is installed -- a Linux host with Mesa's lavapipe shows the guest
/// `Virtio-GPU Venus (llvmpipe (...))` beside its GPU. So a device counts only if what it
/// stands for is not one of those, and a host whose only Vulkan is software has no GPU here.
fn is_gpu(name: &str) -> bool {
    name.strip_prefix("Virtio-GPU Venus (")
        .is_some_and(|host| !SOFTWARE.iter().any(|sw| host.contains(sw)))
}

/// `packages` installed in `sandbox`, from Alpine's repositories.
async fn apk_add(sandbox: &Arc<Sandbox>, packages: &[&str]) {
    let mut cmd = vec!["apk", "add", "--no-cache", "--quiet"];
    cmd.extend(packages);
    let (lines, code) = run(sandbox, &cmd, None).await.unwrap();
    assert_eq!(code, 0, "{lines:#?}");
}

/// The Vulkan devices `sandbox`'s guest enumerates, by name, as `vulkaninfo --summary` lists
/// them; none where it finds no driver to list them with.
async fn vulkan_devices(sandbox: &Arc<Sandbox>) -> Vec<String> {
    let (lines, _) = run(sandbox, &["vulkaninfo", "--summary"], None)
        .await
        .unwrap();
    let devices = lines
        .iter()
        .filter_map(|line| line.trim().strip_prefix("deviceName"))
        .map(|rest| rest.trim_start_matches([' ', '=']).to_owned())
        .collect();
    eprintln!("the guest's Vulkan devices: {devices:?}");
    devices
}

/// A sandbox with a GPU of `memory_mib` (libkrun's 8 GiB for `None`) and the guest half
/// installed.
async fn gpu_sandbox(home: &Home, memory_mib: Option<u32>) -> Arc<Sandbox> {
    let mut args = SandboxCreateArgs::new(GPU_IMAGE).gpu(true);
    if let Some(mib) = memory_mib {
        args = args.gpu_memory_mib(mib);
    }
    let sandbox = home.sandbox(&args).await;
    apk_add(&sandbox, &GPU_GUEST).await;
    sandbox
}

/// `tests/gpu/gpu.c` built in `sandbox` as `/root/gpu`, and its shader as `/root/triple.spv`.
async fn build_gpu_tool(sandbox: &Arc<Sandbox>) {
    apk_add(sandbox, &GPU_BUILD).await;
    for (path, text) in [("/root/gpu.c", GPU_C), ("/root/triple.comp", TRIPLE_COMP)] {
        let write = format!("cat > {path} <<'VIRTX_EOF'\n{text}VIRTX_EOF\n");
        let (lines, code) = run(sandbox, &["sh", "-c", &write], None).await.unwrap();
        assert_eq!(code, 0, "{lines:#?}");
    }
    let build = [
        "sh",
        "-c",
        "glslangValidator -V /root/triple.comp -o /root/triple.spv \
         && cc -O2 -Wall -o /root/gpu /root/gpu.c -lvulkan",
    ];
    let (lines, code) = run(sandbox, &build, None).await.unwrap();
    assert_eq!(code, 0, "{lines:#?}");
}

/// What `/root/gpu compute` printed: one line, `computed <n> on <device>`.
async fn compute(sandbox: &Arc<Sandbox>) -> String {
    let (lines, code) = run(sandbox, &["/root/gpu", "compute", "/root/triple.spv"], None)
        .await
        .unwrap();
    assert_eq!(code, 0, "{lines:#?}");
    assert_eq!(lines.len(), 1, "{lines:#?}");
    lines.into_iter().next().unwrap()
}

/// A sandbox that asks for a GPU finds the host's, through venus: the guest's Vulkan
/// enumerates `Virtio-GPU Venus (<the host's GPU>)`.
async fn a_gpu_sandbox_finds_the_host_s_gpu_through_venus(home: Home) {
    let sandbox = gpu_sandbox(&home, None).await;
    let devices = vulkan_devices(&sandbox).await;
    assert!(
        devices.iter().any(|name| is_gpu(name)),
        "no host GPU among {devices:?}"
    );
    home.sandboxes.delete(sandbox.id()).await.unwrap();
}

/// A sandbox that does not ask for one gets no GPU, in a build that has them and on a host
/// that has one: the same guest, the same driver, and nothing for it to find.
async fn a_sandbox_that_asks_for_no_gpu_finds_none(home: Home) {
    let sandbox = home.sandbox(&SandboxCreateArgs::new(GPU_IMAGE)).await;
    apk_add(&sandbox, &GPU_GUEST).await;
    let devices = vulkan_devices(&sandbox).await;
    assert!(
        !devices.iter().any(|name| name.contains("Venus")),
        "{devices:?}"
    );
    home.sandboxes.delete(sandbox.id()).await.unwrap();
}

/// The GPU's memory is what the sandbox asked for: venus caps every heap the guest's device
/// reports at the window (`VKR_HEAP_SIZE_MAX`), so a guest sizes itself to what it can map
/// rather than to the host device's whole heap.
async fn a_gpu_sandbox_s_heaps_are_the_memory_it_asked_for(home: Home) {
    const MIB: u32 = 2048;
    let sandbox = gpu_sandbox(&home, Some(MIB)).await;
    build_gpu_tool(&sandbox).await;
    let (lines, code) = run(&sandbox, &["/root/gpu", "heaps"], None).await.unwrap();
    assert_eq!(code, 0, "{lines:#?}");
    let sizes: Vec<u64> = lines
        .iter()
        .filter_map(|line| line.split_once("size=")?.1.parse().ok())
        .collect();
    eprintln!("the guest's heaps: {sizes:?}");
    let window = u64::from(MIB) << 20;
    assert!(!sizes.is_empty(), "{lines:#?}");
    assert!(sizes.iter().all(|size| *size <= window), "{sizes:?}");
    assert_eq!(sizes.iter().max(), Some(&window), "{sizes:?}");
    home.sandboxes.delete(sandbox.id()).await.unwrap();
}

/// The guest's GPU does work and hands it back: a compute shader triples-and-adds-one each
/// of a million words in a buffer the guest filled, and the guest reads every one back.
async fn a_gpu_sandbox_runs_a_compute_shader_and_reads_its_answer(home: Home) {
    let sandbox = gpu_sandbox(&home, None).await;
    build_gpu_tool(&sandbox).await;
    let done = compute(&sandbox).await;
    assert!(
        done.strip_prefix("computed 1048576 on ")
            .is_some_and(is_gpu),
        "{done}"
    );
    home.sandboxes.delete(sandbox.id()).await.unwrap();
}

/// What the guest maps of its GPU's memory is mapped through one window the size of that
/// memory: mappings held at once fit until they would pass it, and the one that would is
/// refused with `VK_ERROR_OUT_OF_DEVICE_MEMORY` rather than handed back half-made.
async fn a_gpu_sandbox_s_mappings_end_at_its_memory(home: Home) {
    const OUT_OF_DEVICE_MEMORY: &str = "-2";
    let sandbox = gpu_sandbox(&home, Some(256)).await;
    build_gpu_tool(&sandbox).await;
    let (lines, code) = run(&sandbox, &["/root/gpu", "maps", "100", "3"], None)
        .await
        .unwrap();
    assert_eq!(code, 0, "{lines:#?}");
    eprintln!("100 MiB mappings in a 256 MiB window: {lines:?}");
    assert_eq!(
        lines[..2],
        ["map 0 alloc=0 map=0", "map 1 alloc=0 map=0"],
        "{lines:#?}"
    );
    let third = lines.get(2).expect("a third mapping was tried");
    assert!(
        third.ends_with(&format!("={OUT_OF_DEVICE_MEMORY}")),
        "{lines:#?}"
    );
    home.sandboxes.delete(sandbox.id()).await.unwrap();
}

/// A GPU sandbox parked once unused boots again with its GPU: the device is the sandbox's, not
/// its first machine's.
async fn a_parked_gpu_sandbox_boots_again_with_its_gpu(home: Home) {
    let home = home.parking();
    let sandbox = gpu_sandbox(&home, None).await;
    assert!(
        vulkan_devices(&sandbox)
            .await
            .iter()
            .any(|name| is_gpu(name))
    );
    assert!(until(async || !sandbox.up()).await, "not parked");
    let devices = vulkan_devices(&sandbox).await;
    assert!(
        devices.iter().any(|name| is_gpu(name)),
        "no host GPU among {devices:?}"
    );
    home.sandboxes.delete(sandbox.id()).await.unwrap();
}

/// Sandboxes with GPUs at once each have their own, and each gets its own work back. Each
/// asks for 1 GiB, so two fit beside each other's RAM on any host these run on.
async fn gpu_sandboxes_made_at_once_each_compute_their_own(home: Home) {
    let made = join_all((0..2).map(|_| gpu_sandbox(&home, Some(1024)))).await;
    join_all(made.iter().map(build_gpu_tool)).await;
    let done = join_all(made.iter().map(compute)).await;
    for done in done {
        assert!(
            done.strip_prefix("computed 1048576 on ")
                .is_some_and(is_gpu),
            "{done}"
        );
    }
    join_all(
        made.iter()
            .map(|sandbox| home.sandboxes.delete(sandbox.id())),
    )
    .await;
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
    // Only where `VIRTX_TEST_GPU=1` says the host has a GPU with a Vulkan driver: a `gpu` build
    // runs on hosts without one too, and there a guest finds no device. Said on a build without
    // the feature, the test fails -- the sandbox is refused -- rather than passing on nothing.
    let gpu = env::var_os("VIRTX_TEST_GPU").is_some_and(|v| v == "1");
    runtime.block_on(async {
        let registry = RegistryClient::new(images.clone(), None);
        let pulled: &[(&str, &str)] = if gpu {
            &[(ALPINE, ALPINE_FROM), (GPU_IMAGE, GPU_IMAGE_FROM)]
        } else {
            &[(ALPINE, ALPINE_FROM)]
        };
        for (name, from) in pulled {
            let from = from.parse().unwrap();
            let (_, _, manifest) = registry.pull_manifest(&from).await.unwrap();
            let (_, config) = registry.pull_config(&from, &manifest).await.unwrap();
            let mut layers = Vec::new();
            for descriptor in manifest.layers() {
                let tree = registry.pull_layer(&from, descriptor).await.unwrap();
                layers.push(images.insert_layer(&tree).unwrap());
            }
            let image = images.insert_image(&config, &layers).unwrap();
            images.insert_tag(&name.parse().unwrap(), &image).unwrap();
        }
    });

    type Test = fn(Home) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()>>>;
    macro_rules! tests {
        ($($test:ident),* $(,)?) => {
            [$((stringify!($test), (|home| Box::pin($test(home))) as Test)),*]
        };
    }
    let mut tests = Vec::from(tests![
        a_job_writes_its_output_and_ends_with_its_code_or_its_timeout,
        a_removed_job_is_killed_and_its_sandbox_kept,
        a_job_ends_with_its_process_not_with_what_it_left_running,
        a_job_s_stdin_is_written_until_it_is_closed,
        stdin_a_job_does_not_read_holds_back_its_writer_and_nothing_else,
        an_unused_sandbox_is_parked_and_booted_again_with_what_it_wrote,
        a_sandbox_whose_machine_ends_on_its_own_has_exited_for_good,
        a_running_sandbox_s_disk_is_held_against_another_server,
        a_sandbox_parked_as_it_is_parked_unused_has_synced,
        a_disk_a_parked_machine_left_reads_as_its_kernel_committed,
        sandboxes_made_at_once_each_run_their_own,
        a_sandbox_deleted_twice_at_once_ends_once,
        a_volume_two_sandboxes_mount_is_shared_and_held_until_both_go,
    ]);
    // The Windows guest kernel has no ACPI, so its guest halts rather than powers off.
    if cfg!(not(windows)) {
        tests.extend(tests![
            a_guest_that_powers_itself_off_ends_its_machine_and_its_jobs
        ]);
    }
    if gpu {
        tests.extend(tests![
            a_gpu_sandbox_finds_the_host_s_gpu_through_venus,
            a_sandbox_that_asks_for_no_gpu_finds_none,
            a_gpu_sandbox_s_heaps_are_the_memory_it_asked_for,
            a_gpu_sandbox_runs_a_compute_shader_and_reads_its_answer,
            a_gpu_sandbox_s_mappings_end_at_its_memory,
            a_parked_gpu_sandbox_boots_again_with_its_gpu,
            gpu_sandboxes_made_at_once_each_compute_their_own,
        ]);
    }
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
