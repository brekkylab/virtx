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
    sync::Arc,
    time::Duration,
};

use axum::body::Body;
use futures_util::{StreamExt as _, future::join_all};
use http::{HeaderMap, Method, Request, StatusCode, header};
use virtx_core::{SandboxCreateArgs, SandboxJobEndReason, SandboxJobEntry, SandboxVolume};
use virtx_uvm::{
    DEFAULT_DISK_GIB, Files, ImageStore, RegistryClient, Sandbox, SandboxError, VolumeError,
    VolumeStore,
};

/// What this binary is spawned with to be a machine.
const MACHINE: &str = "vm";

/// The image every sandbox here is made of.
const ALPINE: &str = "alpine:3.20";

/// The image a GPU sandbox is made of: edge, the one Alpine whose repositories carry venus's
/// guest driver (`mesa-vulkan-virtio`); no stable branch does yet. Pulled only for the GPU's
/// test.
const GPU_IMAGE: &str = "alpine:edge";

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
        let sandbox = Sandbox::create(
            &self.images,
            &self.root,
            &self.volumes,
            args,
            vm(),
            park_after,
            keys,
        );
        sandbox.await
    }

    /// The sandboxes kept here, as a server started again over this home knows them.
    fn kept(&self) -> Vec<Arc<Sandbox>> {
        Sandbox::kept(&self.images, &self.root, &self.volumes, vm(), None).known
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
    let sleeping = kept.exec(vec!["sleep".into(), "600".into()], None).await;
    let sleeping = sleeping.unwrap().get().id;
    assert!(
        home.kept().is_empty(),
        "one this process knows is no other's"
    );

    let id = kept.id().to_owned();
    tokio::join!(leave(kept), leave(gone));
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
    let restarted = Some(SandboxJobEndReason::Restarted);
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

    let scratch = env::temp_dir().join(format!("virtx-uvm-machine-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    let runtime = tokio::runtime::Runtime::new().unwrap();
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
        an_idle_sandbox_s_machine_stops_and_boots_again_with_what_it_wrote,
        a_sandbox_closed_while_its_machine_stops_leaves_nothing,
        a_sandbox_s_writable_layer_is_as_large_as_asked,
        sandboxes_made_at_once_each_run_their_own,
        a_sandbox_closed_twice_at_once_ends_once,
        a_kept_sandbox_is_known_again_past_its_server_parked_with_what_it_wrote,
        a_kept_sandbox_whose_machine_ends_on_its_own_has_exited_past_its_server,
        a_kept_sandbox_whose_making_is_cut_short_leaves_nothing,
        a_volume_two_sandboxes_mount_is_shared_and_held_until_both_go,
        a_parked_sandbox_holds_its_volume_against_removal,
        a_put_to_a_sandbox_s_file_answers_the_etag_of_the_file_it_wrote,
        of_two_conditional_writes_to_one_file_in_a_sandbox_one_lands,
        of_two_conditional_writes_to_a_mounted_volume_s_file_one_lands,
    ]);
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
