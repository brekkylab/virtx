//! The tests that boot machines: sandboxes over `virtx-uvm` itself, with no server in front.
//!
//! A machine is a process its binary spawns of itself (see `machine/`), so this test has a
//! `main` of its own: spawned with `vm`, it becomes the machine; otherwise it runs each test.
//! `cargo xtask test -p virtx-uvm` signs it and puts the guest kernel beside it, which a
//! machine needs. The images are pulled once, into a store every test shares; each test keeps
//! its sandboxes' upper disks and its volumes under a home of its own.

use std::{
    collections::HashMap,
    env,
    ffi::OsString,
    panic::{self, AssertUnwindSafe},
    path::{Path, PathBuf},
    process::ExitCode,
    sync::atomic::{AtomicBool, Ordering},
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::body::Body;
use futures_util::{
    StreamExt as _,
    future::{BoxFuture, join_all},
};
use http::{HeaderMap, Method, Request, StatusCode, header};
use virtx_core::{
    SandboxCreateArgs, SandboxJobEnd, SandboxJobEndReason, SandboxJobEntry, SandboxVolume,
};
use virtx_uvm::backend::{Backing, Held, Refusal, Stored, Wanted};
use virtx_uvm::runs::{self, Ran};
use virtx_uvm::{
    Admission, DEFAULT_DISK_GIB, Files, ImageStore, Machine, MachineConfig, MachineError,
    MachineSpec, Network, NoRoom, Reference, RegistryClient, Room, Sandbox, SandboxError,
    SandboxJob, StorageBackend, VolumeError, VolumeMount, VolumeStore,
};

/// What this binary is spawned with to be a machine.
const MACHINE: &str = "vm";

/// What it is spawned with to be a machine whose guest is taken to have stopped once it has
/// answered nothing for 10 s (`VIRTX_GUEST_SILENCE`), not a minute: what a test of that waits on.
const MACHINE_HEARD_BRIEFLY: &str = "vm-heard-briefly";

/// The image every sandbox here is made of.
const ALPINE: &str = "alpine:3.20";

/// The image a GPU sandbox is made of: edge, the one Alpine whose repositories carry venus's
/// guest driver (`mesa-vulkan-virtio`); no stable branch does yet. Pulled only for the GPU's
/// test.
const GPU_IMAGE: &str = "alpine:edge";

/// How long a test waits on what should come before it fails, rather than holding the suite:
/// a guard against a hang, not a time anything is held to.
const HANG: Duration = Duration::from_secs(60);

/// A sandbox of [`ALPINE`] with 1 GiB of memory: enough for what these tests run in it, and little
/// enough that the suite's machines, booting at once, fit a host that commits each one's memory as
/// it boots (`VIRTX_PREFAULT_MEMORY`, as Windows' CI runs them), where 4 GiB each does not.
fn small() -> SandboxCreateArgs {
    let mut args = SandboxCreateArgs::new(ALPINE);
    args.memory_mib = Some(1024);
    args
}

/// The stores a test makes its sandboxes of: the images every test shares, and the upper disks
/// its sandboxes make and its volumes under a home of its own, removed when this drops.
struct Home {
    root: PathBuf,
    images: ImageStore,
    volumes: VolumeStore,
}

impl Home {
    fn new(images: &ImageStore, scratch: &Path, name: &str) -> Home {
        let root = scratch.join(name);
        std::fs::create_dir_all(&root).unwrap();
        Home {
            images: images.clone(),
            volumes: VolumeStore::new(&root).unwrap(),
            root,
        }
    }

    /// A sandbox as `args` asks, its machine stopped once it has been idle for `park_after`.
    async fn sandbox(
        &self,
        args: &SandboxCreateArgs,
        park_after: Option<Duration>,
    ) -> Arc<Sandbox> {
        self.making(args, park_after, None).await.unwrap()
    }

    /// Making a sandbox as `args` asks, kept past this process with `keys` where they are
    /// given.
    async fn making(
        &self,
        args: &SandboxCreateArgs,
        park_after: Option<Duration>,
        keys: Option<HashMap<String, u16>>,
    ) -> Result<Arc<Sandbox>, SandboxError> {
        Sandbox::create(
            &self.images,
            &self.root,
            &self.volumes,
            args,
            vm(),
            park_after,
            keys,
            &Backing::default(),
        )
        .await
    }

    /// The sandboxes kept here, as a server started again over this home knows them.
    fn kept(&self) -> Vec<Arc<Sandbox>> {
        let backing = Backing::default();
        Sandbox::kept(
            &self.images,
            &self.root,
            &self.volumes,
            vm(),
            None,
            &backing,
        )
        .known
    }

    /// The names under `sandbox/` of what sandboxes keep there.
    fn left(&self) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(self.root.join("sandbox")) else {
            return Vec::new();
        };
        let mut left: Vec<_> = entries
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        left
    }
}

/// This binary, as a machine is spawned of it.
fn vm() -> Vec<OsString> {
    vec![env::current_exe().unwrap().into(), OsString::from(MACHINE)]
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

/// Room for as many machines as a test says ([`Limit::to`]), and for every one otherwise: this
/// process's [`Admission`].
struct Limit;

/// How many machines may be up at once, if a test holds them to some; and how many are.
static LIMIT: std::sync::Mutex<(Option<usize>, usize)> = std::sync::Mutex::new((None, 0));

/// A machine's room, given back as it drops.
struct Taken;

impl Drop for Taken {
    fn drop(&mut self) {
        LIMIT.lock().unwrap().1 -= 1;
    }
}

impl Admission for Limit {
    fn admit(&self, _spec: &MachineSpec) -> Result<Room, NoRoom> {
        let mut limit = LIMIT.lock().unwrap();
        if limit.0.is_some_and(|most| limit.1 >= most) {
            return Err(NoRoom(format!("{} machines are up", limit.1)));
        }
        limit.1 += 1;
        Ok(Box::new(Taken))
    }
}

impl Limit {
    /// Machines held to `most` at once, until what this answers drops.
    fn to(most: usize) -> impl Drop {
        struct Lifted;
        impl Drop for Lifted {
            fn drop(&mut self) {
                LIMIT.lock().unwrap().0 = None;
            }
        }
        LIMIT.lock().unwrap().0 = Some(most);
        Lifted
    }

    /// How many machines are up, each holding its room.
    fn taken() -> usize {
        LIMIT.lock().unwrap().1
    }
}

/// Close `sandbox` as a server removes one: it blocks while the guest syncs.
async fn close(sandbox: Arc<Sandbox>) {
    tokio::task::spawn_blocking(move || sandbox.close())
        .await
        .unwrap();
}

/// Let go of `sandbox` as a server that ends does: it blocks while the guest syncs.
async fn leave(sandbox: Arc<Sandbox>) {
    tokio::task::spawn_blocking(move || sandbox.leave())
        .await
        .unwrap();
}

async fn a_kept_sandbox_is_known_again_past_its_server_parked_with_what_it_wrote(home: Home) {
    let keys = HashMap::from([("key".to_owned(), 8080)]);
    let kept = home.making(&small(), None, Some(keys.clone())).await;
    let kept = kept.unwrap();
    let gone = home.sandbox(&small(), None).await;
    let write = ["sh", "-c", "echo kept > /root/kept"];
    assert_eq!(run(&kept, &write, None).await.unwrap().1, 0);
    let running = kept.exec(vec!["sleep".into(), "600".into()], None).await;
    let running = running.unwrap();
    let sleeping = running.get().id;
    let deleted = gone.exec(vec!["sleep".into(), "600".into()], None).await;
    let deleted = deleted.unwrap();
    assert!(
        home.kept().is_empty(),
        "one this process knows is no other's"
    );

    let id = kept.id().to_owned();
    tokio::join!(leave(kept), leave(gone));
    // This server ends each job as the next one knows it: restarted where its sandbox is kept,
    // its sandbox deleted where it is deleted.
    let restarted = Some(SandboxJobEndReason::Restarted);
    assert_eq!(running.get().reason, restarted);
    let sandbox_deleted = Some(SandboxJobEndReason::SandboxDeleted);
    assert_eq!(deleted.get().reason, sandbox_deleted);
    let known = home.kept();
    assert_eq!(known.len(), 1, "the one made without keys goes");
    let again = &known[0];
    assert_eq!(again.id(), id);
    assert!(again.answer().running);
    assert_eq!(again.keys(), keys);
    // Its jobs are kept, the one that ran then ended by the restart, each log its end alone.
    let ended: Vec<_> = again
        .jobs()
        .into_iter()
        .map(|job| (job.id == sleeping, job.running, job.code, job.reason))
        .collect();
    assert!(ended.contains(&(false, false, Some(0), None)), "{ended:?}");
    assert!(
        ended.contains(&(true, false, Some(137), restarted)),
        "{ended:?}"
    );
    let log = again.job(&sleeping).unwrap();
    let log = std::pin::pin!(Arc::clone(log.log()).events(&HeaderMap::new()));
    let log: Vec<_> = log.collect().await;
    let log: Vec<_> = log
        .into_iter()
        .map(|event| String::from_utf8(event.unwrap().to_vec()).unwrap())
        .collect();
    assert_eq!(
        log,
        ["id: 1\ndata: {\"end\":{\"code\":137,\"reason\":\"restarted\"}}\n\n"]
    );
    let (lines, _) = run(again, &["cat", "/root/kept"], None).await.unwrap();
    assert_eq!(lines, ["kept"]);

    // Deleted, it goes with its record.
    close(Arc::clone(again)).await;
    assert_eq!(home.left(), Vec::<String>::new());
}

async fn a_kept_sandbox_whose_machine_ends_on_its_own_has_exited_past_its_server(home: Home) {
    let sandbox = home.making(&small(), None, Some(HashMap::new())).await;
    let sandbox = sandbox.unwrap();
    // A reset, which ends a machine on every arch.
    let reset = run(&sandbox, &["sh", "-c", "sync; reboot -f"], None);
    let ended = tokio::time::timeout(Duration::from_secs(60), reset).await;
    assert_eq!(ended.expect("its machine ended").unwrap().1, 137);
    assert!(!sandbox.answer().running);

    leave(sandbox).await;
    let known = home.kept();
    assert_eq!(known.len(), 1);
    assert!(!known[0].answer().running);
    let after = run(&known[0], &["true"], None).await;
    assert!(matches!(after, Err(SandboxError::Ended)), "{after:?}");
    close(Arc::clone(&known[0])).await;
    assert_eq!(home.left(), Vec::<String>::new());
}

async fn a_kept_sandbox_whose_making_is_cut_short_leaves_nothing(home: Home) {
    // Cut short at each point of its making, from its disk to its boot.
    let args = small();
    for after in [20, 100, 300, 1000] {
        let making = home.making(&args, None, Some(HashMap::new()));
        let made = tokio::time::timeout(Duration::from_millis(after), making).await;
        if let Ok(made) = made {
            close(made.unwrap()).await;
            continue;
        }
        assert!(home.kept().is_empty(), "cut after {after} ms");
        // What it made goes once its machine has, as the next server's sweep finds it.
        let mut left = home.left();
        for _ in 0..300 {
            Sandbox::sweep(&home.root);
            left = home.left();
            if left.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(left, Vec::<String>::new(), "cut after {after} ms");
    }
}

/// A storage backend that keeps nothing and says what it is asked, a line each, as what it is
/// asked to let be written is let go too, a disk with no file under `root` let go of "over
/// nothing"; and refuses a disk while `refusing`.
#[derive(Default)]
struct Told {
    root: PathBuf,
    said: Arc<Mutex<Vec<String>>>,
    refusing: AtomicBool,
}

impl Told {
    fn say(&self, said: String) {
        self.said.lock().unwrap().push(said);
    }

    /// What it was asked since it was last read.
    fn read(&self) -> Vec<String> {
        std::mem::take(&mut *self.said.lock().unwrap())
    }
}

/// Says it is let go of as it drops, and whether over nothing where it holds a file.
struct LetGo(Arc<Mutex<Vec<String>>>, String, Option<PathBuf>);

impl Drop for LetGo {
    fn drop(&mut self) {
        let mut said = std::mem::take(&mut self.1);
        if self.2.as_ref().is_some_and(|file| !file.exists()) {
            said.push_str(" over nothing");
        }
        self.0.lock().unwrap().push(said);
    }
}

impl StorageBackend for Told {
    fn made<'a>(&'a self, what: Stored<'a>, size: u64) -> BoxFuture<'a, Result<(), Refusal>> {
        self.say(format!("made {what:?} {size}"));
        Box::pin(async { Ok(()) })
    }

    fn take<'a>(&'a self, what: Stored<'a>) -> BoxFuture<'a, Result<Held, Refusal>> {
        Box::pin(async move {
            if matches!(what, Stored::Disk(_)) && self.refusing.load(Ordering::SeqCst) {
                return Err(Refusal::InUse(format!("{what:?} is held elsewhere")));
            }
            self.say(format!("take {what:?}"));
            let said = Arc::clone(&self.said);
            let file = match what {
                Stored::Disk(id) => Some(self.root.join("sandbox").join(format!("{id}.ext4"))),
                Stored::Volume(_) => None,
            };
            Ok(Box::new(LetGo(said, format!("let go {what:?}"), file)) as Held)
        })
    }

    fn bring_up<'a>(&'a self, what: Stored<'a>) -> BoxFuture<'a, Result<(), Refusal>> {
        self.say(format!("bring up {what:?}"));
        Box::pin(async { Ok(()) })
    }

    fn want<'a>(&'a self, wanted: Wanted<'a>) -> BoxFuture<'a, anyhow::Result<()>> {
        self.say(match wanted {
            Wanted::Tag(tag) => format!("want {tag}"),
            Wanted::Image(digest) => format!("want {digest}"),
        });
        Box::pin(async { Ok(()) })
    }
}

/// What a backend is asked as a sandbox is made, booted, parked, let go of and taken in again
/// over it: its image wanted, its volumes brought up, and its disk made there; its disk and the
/// volume it writes taken as its machine boots, and let go of once it is parked, the one it
/// reads brought up. Refused, its machine does not boot.
async fn a_sandbox_kept_beyond_its_machine_takes_its_disk_and_volumes_while_it_runs(home: Home) {
    for name in ["rw", "ro"] {
        home.volumes.create(name).unwrap();
    }
    let told = Arc::new(Told {
        root: home.root.clone(),
        ..Told::default()
    });
    let backing = Backing::default();
    assert!(backing.set(Arc::clone(&told) as Arc<dyn StorageBackend>));
    let mut args = small();
    args.volumes = vec![
        SandboxVolume {
            volume: "rw".into(),
            path: "/rw".into(),
            readonly: false,
        },
        SandboxVolume {
            volume: "ro".into(),
            path: "/ro".into(),
            readonly: true,
        },
    ];
    let park = Some(Duration::from_millis(500));
    let keys = Some(HashMap::new());
    let sandbox = Sandbox::create(
        &home.images,
        &home.root,
        &home.volumes,
        &args,
        vm(),
        park,
        keys,
        &backing,
    );
    let sandbox = sandbox.await.unwrap();
    let disk = format!("{:?}", Stored::Disk(sandbox.id()));
    let tag: Reference = ALPINE.parse().unwrap();
    assert_eq!(
        told.read(),
        [
            format!("want {tag}"),
            "bring up Volume(\"rw\")".into(),
            "bring up Volume(\"ro\")".into(),
            format!("made {disk} {}", u64::from(DEFAULT_DISK_GIB) << 30),
            format!("take {disk}"),
            "take Volume(\"rw\")".into(),
            "bring up Volume(\"ro\")".into(),
        ]
    );
    let write = ["sh", "-c", "echo kept > /root/kept && echo kept > /rw/kept"];
    assert_eq!(run(&sandbox, &write, None).await.unwrap().1, 0);

    // Parked, what it took is let go of.
    let let_go = [format!("let go {disk}"), "let go Volume(\"rw\")".into()];
    let parked = async {
        while told.said.lock().unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(30), parked)
        .await
        .expect("parked");
    assert_eq!(told.read(), let_go);

    // Refused its disk, it does not boot; let, it does.
    told.refusing.store(true, Ordering::SeqCst);
    let refused = run(&sandbox, &["true"], None).await;
    assert!(
        matches!(refused, Err(SandboxError::Backend(Refusal::InUse(_)))),
        "{refused:?}"
    );
    assert_eq!(told.read(), Vec::<String>::new());
    told.refusing.store(false, Ordering::SeqCst);

    // Let go of while it runs, and taken in again: its image wanted by its digest, its volumes
    // brought up; parked, with what it wrote.
    let id = sandbox.id().to_owned();
    let image = sandbox.answer().image;
    assert_eq!(run(&sandbox, &["true"], None).await.unwrap().1, 0);
    assert_eq!(told.read().len(), 3, "taken again as it boots");
    leave(sandbox).await;
    assert_eq!(told.read(), let_go);
    let take_in = || {
        Sandbox::take_in(
            &home.images,
            &home.root,
            &home.volumes,
            vm(),
            None,
            &backing,
            &id,
        )
    };
    let again = take_in().await.unwrap();
    assert_eq!(again.id(), id);
    assert!(again.answer().running);
    assert_eq!(
        told.read(),
        [
            format!("want {image}"),
            "bring up Volume(\"rw\")".into(),
            "bring up Volume(\"ro\")".into(),
        ]
    );
    let read = ["cat", "/root/kept", "/rw/kept"];
    let (lines, _) = run(&again, &read, None).await.unwrap();
    assert_eq!(lines, ["kept", "kept"]);
    assert_eq!(told.read().len(), 3, "taken again as it boots");
    let refused = take_in().await;
    assert!(
        matches!(refused, Err(SandboxError::NotFound(_))),
        "one this process knows is taken in once: {:?}",
        refused.err()
    );
    // Closed, what it took is let go of while its disk is still here.
    close(again).await;
    assert_eq!(told.read(), let_go);
    assert_eq!(home.left(), Vec::<String>::new());
}

async fn a_job_writes_its_output_and_ends_with_its_code_or_its_timeout(home: Home) {
    let sandbox = home.sandbox(&small(), None).await;
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
    let sandbox = home.sandbox(&small(), None).await;
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

async fn a_job_s_end_says_why_the_server_ended_it_and_only_then(home: Home) {
    let sandbox = home.sandbox(&small(), None).await;
    let start = async |cmd: &[&str], timeout: Option<Duration>| {
        let cmd = cmd.iter().map(|arg| arg.to_string()).collect();
        sandbox.exec(cmd, timeout).await.unwrap()
    };
    // The last entry of `job`'s log, read to its end.
    let end = async |job: &SandboxJob| {
        let mut events = std::pin::pin!(Arc::clone(job.log()).events(&HeaderMap::new()));
        let mut last = None;
        while let Some(Ok(event)) = events.next().await {
            let event = String::from_utf8(event.to_vec()).unwrap();
            let data = event.lines().find_map(|line| line.strip_prefix("data: "));
            last = Some(serde_json::from_str(data.unwrap()).unwrap());
        }
        match last {
            Some(SandboxJobEntry::End(end)) => end,
            other => panic!("a log ends with its end, not {other:?}"),
        }
    };

    // Its own exit, 137 too: no reason.
    for code in [0, 137] {
        let job = start(&["sh", "-c", &format!("exit {code}")], None).await;
        assert_eq!(end(&job).await, SandboxJobEnd::exited(code));
        assert_eq!(job.get().reason, None);
    }
    // `SIGKILL` sent it: its own end, as the signal made it.
    let job = start(&["sleep", "300"], None).await;
    sandbox.signal(&job.get().id, 9).unwrap();
    assert_eq!(end(&job).await, SandboxJobEnd::exited(137));
    // Past its timeout: killed, for its timeout.
    let job = start(&["sleep", "300"], Some(Duration::from_millis(200))).await;
    let timed_out = SandboxJobEnd {
        code: 137,
        reason: Some(SandboxJobEndReason::Timeout),
    };
    assert_eq!(end(&job).await, timed_out);
    assert_eq!(job.get().reason, Some(SandboxJobEndReason::Timeout));
    // Removed while it runs: its log, read from before, reads on to its end, cancelled.
    let job = start(&["sleep", "300"], None).await;
    let reading = tokio::spawn({
        let job = Arc::clone(&job);
        async move { end(&job).await }
    });
    sandbox.remove_job(&job.get().id).unwrap();
    let read = tokio::time::timeout(HANG, reading)
        .await
        .expect("its log ends");
    let cancelled = SandboxJobEnd {
        code: 137,
        reason: Some(SandboxJobEndReason::Cancelled),
    };
    assert_eq!(read.unwrap(), cancelled);
    // Its machine ended under it -- reset, which every arch takes: killed with it.
    let job = start(&["sh", "-c", "sleep 1; sync; reboot -f"], None).await;
    let ended = tokio::time::timeout(HANG, end(&job))
        .await
        .expect("the machine ends");
    let exited = SandboxJobEnd {
        code: 137,
        reason: Some(SandboxJobEndReason::VmExited),
    };
    assert_eq!(ended, exited);
    assert!(!sandbox.answer().running);
    close(sandbox).await;
    // Its sandbox deleted under it: its log, read from before, reads on to its end, which says
    // so, apart from its own removal.
    let deleting = home.sandbox(&small(), None).await;
    let cmd = ["sleep", "300"].map(String::from).to_vec();
    let job = deleting.exec(cmd, None).await.unwrap();
    let reading = tokio::spawn({
        let job = Arc::clone(&job);
        async move { end(&job).await }
    });
    close(deleting).await;
    let read = tokio::time::timeout(HANG, reading)
        .await
        .expect("its log ends");
    let deleted = SandboxJobEnd {
        code: 137,
        reason: Some(SandboxJobEndReason::SandboxDeleted),
    };
    assert_eq!(read.unwrap(), deleted);
    assert_eq!(job.get().reason, Some(SandboxJobEndReason::SandboxDeleted));
}

/// A machine's process leads a process group of its own, so what is sent its starter's group --
/// a terminal's ctrl-c -- reaches its starter alone, which stops it.
#[cfg(unix)]
async fn a_machine_s_process_leads_a_process_group_of_its_own(home: Home) {
    let sandbox = home.sandbox(&small(), None).await;
    assert_eq!(run(&sandbox, &["true"], None).await.unwrap().1, 0);
    let ps = std::process::Command::new("ps")
        .args(["-A", "-ww", "-o", "pid=,ppid=,pgid=,args="])
        .output()
        .unwrap();
    let ps = String::from_utf8(ps.stdout).unwrap();
    let ours = std::process::id().to_string();
    // Each machine this process started: its pid and its process group.
    let machines: Vec<_> = ps
        .lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>())
        .filter(|fields| fields.get(1) == Some(&ours.as_str()) && fields.get(4) == Some(&MACHINE))
        .map(|fields| (fields[0].to_owned(), fields[2].to_owned()))
        .collect();
    assert_eq!(machines.len(), 1, "{ps}");
    let (pid, group) = &machines[0];
    assert_eq!(group, pid);
    close(sandbox).await;
}

async fn an_idle_sandbox_s_machine_stops_and_boots_again_with_what_it_wrote(home: Home) {
    let park = Some(Duration::from_millis(500));
    let sandbox = home.sandbox(&small(), park).await;
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

    // Its own upper disk goes with it.
    close(sandbox).await;
    let left = std::fs::read_dir(home.root.join("sandbox"))
        .unwrap()
        .count();
    assert_eq!(left, 0);
}

async fn each_machine_s_coming_up_and_going_down_is_told_in_order_with_when(home: Home) {
    let _woken = runs::subscribe();
    let before = runs::since(0, usize::MAX).last;
    let sandbox = home
        .sandbox(&small(), Some(Duration::from_millis(500)))
        .await;
    let first = sandbox.up_since().expect("up as it is made");
    run(&sandbox, &["true"], None).await.unwrap();
    // Idle, it is parked; a job boots it again; closed, it is down for good.
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(sandbox.up_since(), None, "parked");
    run(&sandbox, &["true"], None).await.unwrap();
    let second = sandbox.up_since().expect("up again");
    assert!(second > first);
    let id = sandbox.id().to_owned();
    close(Arc::clone(&sandbox)).await;
    assert_eq!(sandbox.up_since(), None, "closed");

    let told = runs::since(before, usize::MAX);
    assert!(told.all);
    assert_eq!(told.runs.last().map(|run| run.seq), Some(told.last));
    let ours: Vec<_> = (told.runs.into_iter())
        .filter(|run| run.root == home.root)
        .collect();
    assert!(ours.iter().all(|run| run.sandbox == id));
    assert!(ours.windows(2).all(|two| two[0].seq < two[1].seq));
    let ran: Vec<_> = ours.into_iter().map(|run| run.ran).collect();
    let [
        Ran::Up { at: up },
        Ran::Down {
            started,
            ended,
            took,
        },
        Ran::Up { at: again },
        Ran::Down {
            started: started_again,
            ended: ended_again,
            took: took_again,
        },
    ] = ran[..]
    else {
        panic!("up, down, up and down, not {ran:?}");
    };
    assert_eq!((up, started), (first, first));
    assert_eq!((again, started_again), (second, second));
    assert!(started < ended && ended <= again && again < ended_again);
    // How long each was up, by the clock no one sets, is about what the wall clock says.
    for (took, started, ended) in [(took, started, ended), (took_again, again, ended_again)] {
        let wall = ended.duration_since(started).unwrap();
        assert!(
            took.abs_diff(wall) < Duration::from_millis(100),
            "{took:?} by {wall:?}"
        );
    }
}

async fn a_machine_boots_only_with_room_given_back_as_it_goes_down(home: Home) {
    let _limit = Limit::to(1);
    let disks = || {
        let dir = std::fs::read_dir(home.root.join("sandbox")).unwrap();
        let names = dir.map(|entry| entry.unwrap().file_name().into_string().unwrap());
        names.filter(|name| name.ends_with(".ext4")).count()
    };
    let first = home
        .sandbox(&small(), Some(Duration::from_millis(500)))
        .await;
    assert_eq!(Limit::taken(), 1);

    // No room for a second's machine: it is refused, and leaves no disk.
    let vm = vec![env::current_exe().unwrap().into(), OsString::from(MACHINE)];
    let args = small();
    let second = Sandbox::create(
        &home.images,
        &home.root,
        &home.volumes,
        &args,
        vm,
        None,
        None,
        &Backing::default(),
    )
    .await;
    match second {
        Err(SandboxError::NoRoom(NoRoom(why))) => assert_eq!(why, "1 machines are up"),
        other => panic!("expected no room, got {:?}", other.map(|_| ())),
    }
    assert_eq!(disks(), 1);

    // Parked, the first gives its room back, which a second boots in.
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(Limit::taken(), 0);
    let second = home.sandbox(&small(), None).await;
    assert_eq!(Limit::taken(), 1);

    // The first's next job finds no room to boot it again in, until the second is closed.
    let refused = run(&first, &["true"], None).await;
    assert!(
        matches!(refused, Err(SandboxError::NoRoom(_))),
        "{refused:?}"
    );
    close(second).await;
    assert_eq!(Limit::taken(), 0);
    let (lines, _) = run(&first, &["echo", "up"], None).await.unwrap();
    assert_eq!(lines, ["up"]);
    assert_eq!(Limit::taken(), 1);
    close(first).await;
    assert_eq!(Limit::taken(), 0);
}

async fn a_sandbox_is_parked_when_asked_unless_in_use_or_parked_anyway(home: Home) {
    // Never parked by itself: only as it is asked.
    let sandbox = home.sandbox(&small(), None).await;
    run(&sandbox, &["sh", "-c", "echo kept > /root/kept"], None)
        .await
        .unwrap();
    let used = sandbox.last_used();
    assert!(used <= std::time::SystemTime::now());

    // Unused, it is parked; parked, asked again, nothing is done.
    assert!(sandbox.is_up());
    sandbox.park().await.unwrap();
    assert!(!sandbox.is_up());
    sandbox.park().await.unwrap();

    // A job boots it again, over what it wrote; while one runs, it is refused.
    let cmd = ["sleep", "30"].map(String::from).to_vec();
    let job = sandbox.exec(cmd, None).await.unwrap();
    assert!(sandbox.is_up());
    assert!(matches!(sandbox.park().await, Err(SandboxError::InUse(_))));
    assert!(sandbox.is_up());
    assert!(job.get().running);

    // Parked anyway, what ran in it ends, killed.
    sandbox.park_anyway().await;
    assert!(!sandbox.is_up());
    // Its end is read off its log, which the machine's going down ends.
    let mut events = std::pin::pin!(Arc::clone(job.log()).events(&HeaderMap::new()));
    let ended = tokio::time::timeout(Duration::from_secs(60), async {
        while let Some(Ok(event)) = events.next().await {
            let event = String::from_utf8(event.to_vec()).unwrap();
            if let Some(data) = event.lines().find_map(|line| line.strip_prefix("data: "))
                && let Ok(SandboxJobEntry::End(end)) = serde_json::from_str(data)
            {
                return end.code;
            }
        }
        panic!("a job's log ends with its end")
    });
    assert_eq!(ended.await.expect("the job went on"), 128 + 9);
    assert!(!job.get().running);
    let (lines, _) = run(&sandbox, &["cat", "/root/kept"], None).await.unwrap();
    assert_eq!(lines, ["kept"]);
    assert!(sandbox.last_used() >= used);
    close(Arc::clone(&sandbox)).await;
    // Closed, it is ended.
    assert!(matches!(sandbox.park().await, Err(SandboxError::Ended)));
}

async fn a_guest_that_powers_itself_off_ends_its_machine_and_its_jobs(home: Home) {
    let sandbox = home
        .sandbox(&small(), Some(Duration::from_millis(500)))
        .await;
    let off = ["sh", "-c", "sleep 1; poweroff -f"];
    let ended = tokio::time::timeout(Duration::from_secs(60), run(&sandbox, &off, None)).await;
    // Its job never says it ended: it ends as killed, with the machine.
    assert_eq!(ended.expect("the machine ends").unwrap().1, 137);
    assert!(sandbox.idle().is_some());
    let after = run(&sandbox, &["true"], None).await;
    assert!(matches!(after, Err(SandboxError::Ended)), "{after:?}");
    close(sandbox).await;
}

/// A guest that halts rather than powering off, as `halt -f` does, leaves the machine's
/// process running and answers nothing: once it has been silent past the bound, its machine is
/// ended and its jobs with it, as on a power-off. One that is up answers, however long its
/// job runs quietly and however busy it keeps it.
async fn a_guest_that_halts_ends_its_machine_and_its_jobs(home: Home) {
    let vm = vec![
        env::current_exe().unwrap().into(),
        OsString::from(MACHINE_HEARD_BRIEFLY),
    ];
    let park_after = Some(Duration::from_millis(500));
    let args = small();
    let sandbox = Sandbox::create(
        &home.images,
        &home.root,
        &home.volumes,
        &args,
        vm,
        park_after,
        None,
        &Backing::default(),
    )
    .await
    .unwrap();
    let busy = [
        "sh",
        "-c",
        "for i in 1 2; do timeout 15 sh -c 'while :; do :; done' & done; wait; true",
    ];
    let lived = run(&sandbox, &busy, None).await;
    assert_eq!(lived.unwrap().1, 0, "a busy guest is ended");
    let halt = ["sh", "-c", "sleep 1; halt -f"];
    let ended = tokio::time::timeout(Duration::from_secs(60), run(&sandbox, &halt, None)).await;
    assert_eq!(ended.expect("the machine ends").unwrap().1, 137);
    assert!(sandbox.idle().is_some());
    let after = run(&sandbox, &["true"], None).await;
    assert!(matches!(after, Err(SandboxError::Ended)), "{after:?}");
    close(sandbox).await;
}

async fn a_sandbox_closed_while_its_machine_stops_leaves_nothing(home: Home) {
    // The machine is parked about 500 ms after its last job ends, and syncs for a hundred or
    // so: one close per delay across that.
    for delay in (450..=700).step_by(50) {
        let args = small();
        let sandbox = home.sandbox(&args, Some(Duration::from_millis(500))).await;
        // Left in the guest's page cache, so stopping the machine takes the while it syncs.
        let big = ["dd", "if=/dev/zero", "of=/root/big", "bs=1M", "count=256"];
        run(&sandbox, &big, None).await.unwrap();

        tokio::time::sleep(Duration::from_millis(delay)).await;
        close(sandbox).await;
        let left = std::fs::read_dir(home.root.join("sandbox"))
            .unwrap()
            .count();
        assert_eq!(left, 0, "closed after {delay} ms");
    }
}

async fn a_sandbox_s_writable_layer_is_as_large_as_asked(home: Home) {
    let asked = [(DEFAULT_DISK_GIB, small()), (8, small().disk_gib(8))];
    for (gib, args) in asked {
        let sandbox = home.sandbox(&args, None).await;
        // The root is the overlay, whose size is its writable layer's.
        let df = ["sh", "-c", "df -k / | tail -n 1 | awk '{print $2}'"];
        let (lines, _) = run(&sandbox, &df, None).await.unwrap();
        let kib: u64 = lines[0].parse().unwrap();
        // Less what ext4 keeps for itself.
        let size = u64::from(gib) << 20;
        assert!(
            kib <= size && kib > size * 9 / 10,
            "{kib} KiB for {gib} GiB"
        );
        close(sandbox).await;
    }
}

async fn sandboxes_made_at_once_each_run_their_own(home: Home) {
    let args = small();
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
    let sandbox = home.sandbox(&small(), None).await;
    let cmd = ["sleep", "30"].map(String::from).to_vec();
    let job = sandbox.exec(cmd, None).await.unwrap();
    tokio::join!(close(Arc::clone(&sandbox)), close(Arc::clone(&sandbox)));
    assert!(!sandbox.answer().running);
    assert!(!job.get().running);
    assert!(sandbox.jobs().is_empty());
    let after = run(&sandbox, &["true"], None).await;
    assert!(matches!(after, Err(SandboxError::Ended)), "{after:?}");
    let left = std::fs::read_dir(home.root.join("sandbox"))
        .unwrap()
        .count();
    assert_eq!(left, 0);
}

/// A machine whose volume does not open does not start, and says why to a caller elsewhere
/// without the volume's path, which its whole account names.
async fn a_machine_that_did_not_start_tells_why_without_this_host_s_paths(home: Home) {
    let volume = home.volumes.create("broken").unwrap();
    std::fs::write(volume.path().join("mounts.json"), "not a table").unwrap();
    let config = MachineConfig {
        rootfs: home.root.join("rootfs.vmdk"),
        config: Default::default(),
        upper: home.root.join("upper.ext4"),
        spec: MachineSpec {
            vcpus: 1,
            ram_mib: 512,
            gpu: false,
            gpu_memory_mib: None,
            network: Network::None,
        },
        volumes: vec![VolumeMount {
            path: volume.path().to_path_buf(),
            at: "/data".into(),
            read_only: false,
        }],
        abin: None,
        ports: Vec::new(),
    };
    let started = tokio::task::spawn_blocking(move || Machine::start(config, &vm()))
        .await
        .unwrap();
    let (account, told) = match started {
        Err(MachineError::DidNotStart { account, told }) => (account, told),
        Err(e) => panic!("{e}"),
        Ok(_) => panic!("a machine whose volume does not open started"),
    };
    let path = volume.path().display().to_string();
    assert!(account.contains(&path), "{account}");
    assert!(!told.contains(&path), "{told}");
    assert!(
        told.ends_with("\nthe server's files: other error"),
        "{told}"
    );
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

/// A machine with more virtio-mmio devices than the 16 legacy interrupt lines hold boots, and
/// each answers: five volumes over the eight devices this sandbox has without them, the last
/// volume and the guest agent's filesystem on lines past 15.
async fn a_sandbox_mounting_many_volumes_reaches_each(home: Home) {
    let names = ["v1", "v2", "v3", "v4", "v5"];
    for name in names {
        home.volumes.create(name).unwrap();
    }
    let args = SandboxCreateArgs {
        volumes: names
            .iter()
            .map(|name| SandboxVolume {
                volume: name.to_string(),
                path: format!("/{name}"),
                readonly: false,
            })
            .collect(),
        ..small()
    };
    let sandbox = home.sandbox(&args, None).await;
    let write = [
        "sh",
        "-c",
        "for v in v1 v2 v3 v4 v5; do echo $v > /$v/name; done",
    ];
    assert_eq!(run(&sandbox, &write, None).await.unwrap().1, 0);
    close(sandbox).await;
    for name in names {
        let written = home.volumes.open(name).unwrap().files().join("name");
        assert_eq!(
            std::fs::read_to_string(written).unwrap(),
            format!("{name}\n")
        );
    }
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
async fn gpu_sandbox(home: &Home, memory_mib: Option<u32>, park: Option<Duration>) -> Arc<Sandbox> {
    let mut args = SandboxCreateArgs::new(GPU_IMAGE).gpu(true);
    if let Some(mib) = memory_mib {
        args = args.gpu_memory_mib(mib);
    }
    let sandbox = home.sandbox(&args, park).await;
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
    let sandbox = gpu_sandbox(&home, None, None).await;
    let devices = vulkan_devices(&sandbox).await;
    assert!(
        devices.iter().any(|name| is_gpu(name)),
        "no host GPU among {devices:?}"
    );
    close(sandbox).await;
}

/// A sandbox that does not ask for one gets no GPU, in a build that has them and on a host
/// that has one: the same guest, the same driver, and nothing for it to find.
async fn a_sandbox_that_asks_for_no_gpu_finds_none(home: Home) {
    let sandbox = home.sandbox(&SandboxCreateArgs::new(GPU_IMAGE), None).await;
    apk_add(&sandbox, &GPU_GUEST).await;
    let devices = vulkan_devices(&sandbox).await;
    assert!(
        !devices.iter().any(|name| name.contains("Venus")),
        "{devices:?}"
    );
    close(sandbox).await;
}

/// The GPU's memory is what the sandbox asked for: venus caps every heap the guest's device
/// reports at the window (`VKR_HEAP_SIZE_MAX`), so a guest sizes itself to what it can map
/// rather than to the host device's whole heap.
async fn a_gpu_sandbox_s_heaps_are_the_memory_it_asked_for(home: Home) {
    const MIB: u32 = 2048;
    let sandbox = gpu_sandbox(&home, Some(MIB), None).await;
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
    close(sandbox).await;
}

/// The guest's GPU does work and hands it back: a compute shader triples-and-adds-one each
/// of a million words in a buffer the guest filled, and the guest reads every one back.
async fn a_gpu_sandbox_runs_a_compute_shader_and_reads_its_answer(home: Home) {
    let sandbox = gpu_sandbox(&home, None, None).await;
    build_gpu_tool(&sandbox).await;
    let done = compute(&sandbox).await;
    assert!(
        done.strip_prefix("computed 1048576 on ")
            .is_some_and(is_gpu),
        "{done}"
    );
    close(sandbox).await;
}

/// What the guest maps of its GPU's memory is mapped through one window the size of that
/// memory: mappings held at once fit until they would pass it, and the one that would is
/// refused with `VK_ERROR_OUT_OF_DEVICE_MEMORY` rather than handed back half-made.
async fn a_gpu_sandbox_s_mappings_end_at_its_memory(home: Home) {
    const OUT_OF_DEVICE_MEMORY: &str = "-2";
    let sandbox = gpu_sandbox(&home, Some(256), None).await;
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
    close(sandbox).await;
}

/// A GPU sandbox whose machine stops when idle boots again with its GPU: the device is the
/// sandbox's, not its first machine's.
async fn a_parked_gpu_sandbox_boots_again_with_its_gpu(home: Home) {
    let sandbox = gpu_sandbox(&home, None, Some(Duration::from_millis(500))).await;
    assert!(
        vulkan_devices(&sandbox)
            .await
            .iter()
            .any(|name| is_gpu(name))
    );
    // What runs past its job ends with the machine, so seeing it gone is seeing it stopped.
    run(
        &sandbox,
        &["sh", "-c", "sleep 1000 >/dev/null 2>&1 &"],
        None,
    )
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    let (lines, _) = run(&sandbox, &["sh", "-c", "ps | grep -c '[s]leep 1000'"], None)
        .await
        .unwrap();
    assert_eq!(lines, ["0"], "the machine did not stop");
    let devices = vulkan_devices(&sandbox).await;
    assert!(
        devices.iter().any(|name| is_gpu(name)),
        "no host GPU among {devices:?}"
    );
    close(sandbox).await;
}

/// Sandboxes with GPUs at once each have their own, and each gets its own work back. Each
/// asks for 1 GiB, so two fit beside each other's RAM on any host these run on.
async fn gpu_sandboxes_made_at_once_each_compute_their_own(home: Home) {
    let made = join_all((0..2).map(|_| gpu_sandbox(&home, Some(1024), None))).await;
    join_all(made.iter().map(build_gpu_tool)).await;
    let done = join_all(made.iter().map(compute)).await;
    for done in done {
        assert!(
            done.strip_prefix("computed 1048576 on ")
                .is_some_and(is_gpu),
            "{done}"
        );
    }
    join_all(made.into_iter().map(close)).await;
}

async fn a_parked_sandbox_holds_its_volume_against_removal(home: Home) {
    home.volumes.create("data").unwrap();
    let mounting = SandboxCreateArgs {
        volumes: vec![SandboxVolume {
            volume: "data".into(),
            path: "/data".into(),
            readonly: false,
        }],
        ..small()
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

async fn a_put_to_a_sandbox_s_file_answers_the_etag_of_the_file_it_wrote(home: Home) {
    let sandbox = home.sandbox(&small(), None).await;
    let files = Files::new(home.volumes.clone());
    let sent = async |method: Method, if_match: Option<&str>, body: &'static str| {
        let mut request = Request::builder()
            .method(method)
            .uri("/sandboxes/sb/files/tmp/a.txt");
        if let Some(etag) = if_match {
            request = request.header(header::IF_MATCH, etag);
        }
        let request = request.body(Body::from(body)).unwrap();
        let machine = sandbox.machine().await.unwrap();
        let response = files
            .sandbox("sb", machine, sandbox.activity(), request)
            .await;
        let etag = response.headers().get(header::ETAG);
        (
            response.status(),
            etag.map(|etag| etag.to_str().unwrap().to_owned()),
        )
    };

    let (status, put) = sent(Method::PUT, None, "one").await;
    assert_eq!(status, StatusCode::CREATED);
    let put = put.expect("a PUT answers an ETag");
    let (status, got) = sent(Method::GET, None, "").await;
    assert_eq!(
        (status, got.as_deref()),
        (StatusCode::OK, Some(put.as_str()))
    );

    // The tag a write was answered is the one the next write is taken over. That write is
    // longer: a tag is a file's length and time, and the guest's file times move a clock tick at
    // a time, so two writes in a row can have one time.
    let (status, again) = sent(Method::PUT, Some(&put), "three").await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, got) = sent(Method::GET, None, "").await;
    assert_eq!(got, again);
    let (status, _) = sent(Method::PUT, Some(&put), "old").await;
    assert_eq!(status, StatusCode::PRECONDITION_FAILED);
    close(sandbox).await;
}

/// A `PATCH` of a sandbox's file is not allowed, `405` with the `Allow` an `OPTIONS` of it
/// answers, and leaves the file as it was; a directory is not read with a `GET`.
async fn a_patch_of_a_sandbox_s_file_is_not_allowed_and_leaves_it(home: Home) {
    use http_body_util::BodyExt as _;
    let sandbox = home.sandbox(&small(), None).await;
    let files = Files::new(home.volumes.clone());
    let sent = async |method: Method, path: &str, body: &'static str| {
        let request = Request::builder()
            .method(method)
            .uri(format!("/sandboxes/sb/files/{path}"))
            .body(Body::from(body))
            .unwrap();
        let machine = sandbox.machine().await.unwrap();
        let response = files
            .sandbox("sb", machine, sandbox.activity(), request)
            .await;
        let (status, allow) = (
            response.status(),
            response.headers().get(header::ALLOW).cloned(),
        );
        let body = response.into_body().collect().await.unwrap().to_bytes();
        (status, allow, body)
    };

    let (status, _, _) = sent(Method::PUT, "tmp/a.txt", "0123456789").await;
    assert_eq!(status, StatusCode::CREATED);
    let (_, allow, _) = sent(Method::OPTIONS, "tmp/a.txt", "").await;
    let allow = allow.expect("OPTIONS answers Allow");
    assert!(!allow.to_str().unwrap().contains("PATCH"), "{allow:?}");
    let (status, refused, body) = sent(Method::PATCH, "tmp/a.txt", "YY").await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(refused, Some(allow));
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["error"]["code"], "method_not_allowed", "{body}");
    let (_, _, read) = sent(Method::GET, "tmp/a.txt", "").await;
    assert_eq!(read, "0123456789");
    let (status, _, _) = sent(Method::GET, "", "").await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "GET of a directory");
    close(sandbox).await;
}

/// Two conditional writes to one file in a sandbox at once: the second, sent while the first is
/// still sending its body, is not held up by it and lands, and the first, checked as it lands
/// against what the second wrote, is refused `412`. For `If-None-Match: *` to a new file, and
/// for one `If-Match` to a file there.
async fn of_two_conditional_writes_to_one_file_in_a_sandbox_one_lands(home: Home) {
    use axum::body::Body;
    use http::{Request, header};
    use http_body_util::BodyExt as _;

    let sandbox = home.sandbox(&small(), None).await;
    let files = Arc::new(Files::new(home.volumes.clone()));
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
        let (slow, read, go) = stalled();
        let first = tokio::spawn(send(put(slow)));
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
    use axum::body::Body;
    use http::{Request, header};
    use http_body_util::BodyExt as _;

    home.volumes.create("data").unwrap();
    let mounting = SandboxCreateArgs {
        volumes: vec![SandboxVolume {
            volume: "data".into(),
            path: "/data".into(),
            readonly: false,
        }],
        ..small()
    };
    let sandbox = home.sandbox(&mounting, None).await;
    let files = Arc::new(Files::new(home.volumes.clone()));
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
                false => files.volume("data", request).await,
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
            let (slow, read, go) = stalled();
            let first = put(first_in_sandbox, slow);
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

    // Paths the volume's own files do not answer for, which the guest does: through a link in
    // the volume, and by a name with a backslash, which the volume refuses. Each is checked as
    // the guest sees it, so a tag the guest answered is taken, and of two at once one lands.
    let made = ["sh", "-c", "mkdir /data/real && ln -s real /data/link"];
    assert_eq!(run(&sandbox, &made, None).await.unwrap().1, 0);
    // On a Windows host a mounted volume does not take a name with a backslash.
    let names: &[&str] = if cfg!(windows) {
        &["link/f"]
    } else {
        &["link/f", "a%5Cb"]
    };
    for &name in names {
        let put = Request::put(at(true, name)).body(Body::from("one"));
        assert_eq!(send(true, put.unwrap()).await.status(), 201, "{name}");
        let etag = tag(true, name.to_owned()).await;
        let put = Request::put(at(true, name))
            .header(header::IF_MATCH, &etag)
            .body(Body::from("two"));
        assert_eq!(send(true, put.unwrap()).await.status(), 204, "{name}");
    }
    let fresh = |name: &str, body: Body| {
        Request::put(at(true, name))
            .header(header::IF_NONE_MATCH, "*")
            .body(body)
            .unwrap()
    };
    if !cfg!(windows) {
        assert_eq!(
            send(true, fresh("c%5Cd", Body::from("c"))).await.status(),
            201
        );
    }
    let (slow, read, go) = stalled();
    let first = tokio::spawn(send(true, fresh("link/g", slow)));
    read.await.unwrap();
    assert_eq!(
        send(true, fresh("link/g", Body::from("second")))
            .await
            .status(),
        201
    );
    go.send(()).unwrap();
    assert_eq!(first.await.unwrap().status(), 412, "link/g");

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

/// A body still arriving: a part, said so on the first receiver once it is read, then the rest
/// once the sender is let go.
fn stalled() -> (
    axum::body::Body,
    tokio::sync::oneshot::Receiver<()>,
    tokio::sync::oneshot::Sender<()>,
) {
    use axum::body::Bytes;
    let (sending, read) = tokio::sync::oneshot::channel();
    let (go, let_go) = tokio::sync::oneshot::channel::<()>();
    let slow = futures_util::stream::unfold((Some(sending), Some(let_go)), |state| async move {
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
    (axum::body::Body::from_stream(slow), read, go)
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
    if env::args_os()
        .nth(1)
        .is_some_and(|arg| arg == MACHINE_HEARD_BRIEFLY)
    {
        // SAFETY: nothing else runs in this process yet.
        unsafe { env::set_var("VIRTX_GUEST_SILENCE", "10") };
        let Err(_) = virtx_uvm::enter_machine();
        std::process::exit(1);
    }

    let scratch = env::temp_dir().join(format!("virtx-uvm-machine-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    assert!(virtx_uvm::set_admission(Arc::new(Limit)));
    let images = ImageStore::new(scratch.join("images")).unwrap();
    // Only where `VIRTX_TEST_GPU=1` says the host has a GPU with a Vulkan driver: a `gpu` build
    // runs on hosts without one too, and there a guest finds no device. Said on a build without
    // the feature, the test fails -- the sandbox is refused -- rather than passing on nothing.
    let gpu = env::var_os("VIRTX_TEST_GPU").is_some_and(|v| v == "1");
    let pulled: &[&str] = if gpu { &[ALPINE, GPU_IMAGE] } else { &[ALPINE] };
    runtime.block_on(async {
        let registry = RegistryClient::new(images.clone(), None);
        for name in pulled {
            let reference = name.parse().unwrap();
            let (_, _, manifest) = registry.pull_manifest(&reference).await.unwrap();
            let (_, config) = registry.pull_config(&reference, &manifest).await.unwrap();
            let mut layers = Vec::new();
            for descriptor in manifest.layers() {
                let tree = registry.pull_layer(&reference, descriptor).await.unwrap();
                layers.push(images.insert_layer(&tree).unwrap());
            }
            let image = images.insert_image(&config, &layers).unwrap();
            images.insert_tag(&reference, &image).unwrap();
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
        a_job_s_end_says_why_the_server_ended_it_and_only_then,
        an_idle_sandbox_s_machine_stops_and_boots_again_with_what_it_wrote,
        each_machine_s_coming_up_and_going_down_is_told_in_order_with_when,
        a_machine_boots_only_with_room_given_back_as_it_goes_down,
        a_sandbox_is_parked_when_asked_unless_in_use_or_parked_anyway,
        a_guest_that_halts_ends_its_machine_and_its_jobs,
        a_sandbox_closed_while_its_machine_stops_leaves_nothing,
        a_sandbox_s_writable_layer_is_as_large_as_asked,
        sandboxes_made_at_once_each_run_their_own,
        a_sandbox_closed_twice_at_once_ends_once,
        a_kept_sandbox_is_known_again_past_its_server_parked_with_what_it_wrote,
        a_kept_sandbox_whose_machine_ends_on_its_own_has_exited_past_its_server,
        a_kept_sandbox_whose_making_is_cut_short_leaves_nothing,
        a_sandbox_kept_beyond_its_machine_takes_its_disk_and_volumes_while_it_runs,
        a_machine_that_did_not_start_tells_why_without_this_host_s_paths,
        a_volume_two_sandboxes_mount_is_shared_and_held_until_both_go,
        a_sandbox_mounting_many_volumes_reaches_each,
        a_parked_sandbox_holds_its_volume_against_removal,
        a_put_to_a_sandbox_s_file_answers_the_etag_of_the_file_it_wrote,
        a_patch_of_a_sandbox_s_file_is_not_allowed_and_leaves_it,
        of_two_conditional_writes_to_one_file_in_a_sandbox_one_lands,
        of_two_conditional_writes_to_a_mounted_volume_s_file_one_lands,
    ]);
    #[cfg(unix)]
    tests.extend(tests![a_machine_s_process_leads_a_process_group_of_its_own]);
    // The Windows guest kernel has no ACPI, so its guest halts rather than powers off, and is
    // ended only once it has been silent, as one that halts.
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
