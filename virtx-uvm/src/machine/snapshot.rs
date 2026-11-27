//! The snapshots machines are restored from: one per shape, kept under a home's `snapshots/` as
//! `<host>/<shape>`, which the first machine of the shape takes as its agent waits and every
//! later one is restored from. Taken and restored on x86_64 Linux alone.

use std::fs::{self, File};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use super::MachineConfig;
use super::wire;

/// Names what restores a snapshot here: `program`, whose libkrun and agent write it, and this
/// host's CPU and kernel, whose KVM it names the features of. None off x86_64 Linux, where no
/// machine is restored.
pub(crate) fn host(program: &Path) -> Option<String> {
    if !cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        return None;
    }
    // The same for the process's life, and read once: every machine it starts asks.
    static MACHINE: OnceLock<Option<u64>> = OnceLock::new();
    let machine = (*MACHINE.get_or_init(cpu_and_kernel))?;
    let program = fs::metadata(program).ok()?;
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    machine.hash(&mut hash);
    (program.len(), program.modified().ok()).hash(&mut hash);
    Some(format!("{:016x}", hash.finish()))
}

/// This host's kernel release and its first CPU's identity, which every other's matches.
fn cpu_and_kernel() -> Option<u64> {
    use std::io::BufRead;
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    fs::read("/proc/sys/kernel/osrelease").ok()?.hash(&mut hash);
    // The first CPU's alone: the whole file is slow to make on a host of many.
    std::io::BufReader::new(File::open("/proc/cpuinfo").ok()?)
        .lines()
        .map_while(|line| line.ok())
        .take_while(|line| !line.is_empty())
        .filter(|line| {
            [
                "vendor_id",
                "cpu family",
                "model",
                "stepping",
                "microcode",
                "flags",
            ]
            .iter()
            .any(|key| line.split(':').next().is_some_and(|k| k.trim() == *key))
        })
        .for_each(|line| line.hash(&mut hash));
    Some(hash.finish())
}

/// Where the snapshot of `machine`'s shape is kept on `host`: the shape is everything a restored
/// guest has to find as it was -- its vCPUs, memory, devices, root image, environment and upper
/// disk's size. None for a machine that keeps no snapshots ([`MachineConfig::snapshots`]), for a
/// GPU's, and off x86_64 Linux.
pub(super) fn shape(
    machine: &MachineConfig,
    info: &wire::MachineInfo,
    env: &[String],
    host: Option<&str>,
) -> Option<PathBuf> {
    let dir = machine.snapshots.as_ref()?.join(host?);
    if machine.spec.gpu {
        return None;
    }
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    machine.spec.vcpus.hash(&mut hash);
    machine.spec.ram_mib.hash(&mut hash);
    (machine.spec.network || !machine.ports.is_empty()).hash(&mut hash);
    info.shares.hash(&mut hash);
    machine.rootfs.hash(&mut hash);
    // The agent's, which every command inherits: an image's own though another has its layers.
    env.hash(&mut hash);
    fs::metadata(&machine.upper).ok()?.len().hash(&mut hash);
    fs::create_dir_all(&dir).ok()?;
    Some(dir.join(format!("{:016x}", hash.finish())))
}

/// What a machine does with its shape's snapshot at `dir`, as `(save, restore)`: takes it as it
/// boots on, is restored from it, or neither once its restores have not come up.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub(super) fn use_of(dir: &Path) -> (Option<PathBuf>, Option<PathBuf>) {
    if dir.join(BROKEN).exists() {
        (None, None)
    } else if !dir.join("state").exists() {
        (Some(dir.to_owned()), None)
    } else if restores_lost(dir) {
        broken(dir);
        (None, None)
    } else {
        (None, Some(dir.to_owned()))
    }
}

/// Off x86_64 Linux there is no snapshot, nor a shape that names one.
#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
pub(super) fn use_of(_: &Path) -> (Option<PathBuf>, Option<PathBuf>) {
    (None, None)
}

/// Beside a snapshot whose restores do not come up, which a machine of its shape then boots
/// without and takes none of.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const BROKEN: &str = "broken";

/// How many restores of one snapshot may end before their agent is ready, with none ready since,
/// before it is taken for broken: one that failed to build, or was cut short as its sandbox
/// closed, may have been the host's moment rather than the snapshot's.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const RESTORES_LOST: usize = 3;

/// `dir` is being restored from by this process, until [`restore_ready`]: what [`sweep`] leaves
/// for another [`KEPT`], and what counts against it if this process ends first.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub(super) fn restoring(dir: &Path) {
    let _ = File::open(dir).and_then(|dir| dir.set_modified(std::time::SystemTime::now()));
    let _ = File::create(restoring_at(dir, std::process::id()));
}

/// Named in a snapshot by the process restoring it, until its agent is ready.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn restoring_at(dir: &Path, pid: u32) -> PathBuf {
    dir.join(format!("restoring-{pid}"))
}

/// The restores of `dir` whose process has ended before their agent was ready.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn lost_restores(dir: &Path) -> Vec<PathBuf> {
    fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .and_then(|name| name.strip_prefix("restoring-"))
                .and_then(|pid| pid.parse::<libc::pid_t>().ok())
                // SAFETY: signal 0 is only asked whether the process is there. Another user's
                // answers EPERM, and is there.
                .is_some_and(|pid| {
                    (unsafe { libc::kill(pid, 0) }) != 0
                        && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
                })
        })
        .map(|entry| entry.path())
        .collect()
}

/// Whether `dir`'s restores keep ending before their agent is ready.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn restores_lost(dir: &Path) -> bool {
    lost_restores(dir).len() >= RESTORES_LOST
}

/// A restore of `dir` has come up: it and those lost before it are forgotten.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub(super) fn restore_ready(dir: &Path) {
    let _ = fs::remove_file(restoring_at(dir, std::process::id()));
    for lost in lost_restores(dir) {
        let _ = fs::remove_file(lost);
    }
}

/// Marks `dir` broken and removes all of it but its root image's name, which [`sweep`] reads.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn broken(dir: &Path) {
    log::error!("snapshot {}: its restores do not come up", dir.display());
    let _ = File::create(dir.join(BROKEN));
    for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
        let name = entry.file_name();
        if name != "rootfs" && name != BROKEN {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// How long a snapshot no machine has been restored from is kept once none can be: the
/// restores a server of another binary sharing the home may still be making from it.
const KEPT: Duration = Duration::from_secs(60 * 60);

/// How long a snapshot a machine here could be restored from is kept with none restored from it:
/// a shape not made for a day costs one boot in full again, not its memory on disk meanwhile.
const UNUSED: Duration = Duration::from_secs(24 * 60 * 60);

/// How much of the disk a home's snapshots may hold, past which the least lately used go.
const HELD: u64 = 1 << 30;

/// How lately a snapshot taken or restored from is left out of what goes for [`HELD`]: a restore
/// that has just chosen it has yet to read it.
const IN_USE: Duration = Duration::from_secs(60);

/// Removes from `snapshots`, a home's ([`MachineConfig::snapshots`]), what no machine of `host`
/// needs: once none has been taken or restored from for [`KEPT`], each it cannot be restored
/// from -- another binary's or host's, one whose root image is gone, one left half written;
/// each none has been restored from for [`UNUSED`]; and, while the rest hold more than [`HELD`],
/// the least lately used.
pub(crate) fn sweep(snapshots: &Path, host: Option<&str>) {
    sweep_to(snapshots, host, HELD);
}

fn sweep_to(snapshots: &Path, host: Option<&str>, held: u64) {
    let entries = |dir: &Path| {
        fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .collect::<Vec<_>>()
    };
    let idle = |dir: &Path| {
        fs::metadata(dir)
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|at| at.elapsed().ok())
            .unwrap_or_default()
    };
    // Those kept, as how long each has been idle, what it holds and where.
    let mut kept = Vec::new();
    for binary in entries(snapshots) {
        let ours = host.is_some_and(|host| binary.file_name() == Some(host.as_ref()));
        for snapshot in entries(&binary) {
            let idle = idle(&snapshot);
            let unusable = !ours
                || snapshot.extension().is_some()
                || fs::read(snapshot.join("rootfs")).is_ok_and(|rootfs| {
                    !Path::new(&*String::from_utf8_lossy(&rootfs)).exists()
                });
            if (idle > KEPT && unusable) || idle > UNUSED {
                let _ = fs::remove_dir_all(&snapshot);
            } else {
                kept.push((idle, held_by(&snapshot), snapshot));
            }
        }
        if !ours {
            let _ = fs::remove_dir(&binary);
        }
    }
    let mut holding: u64 = kept.iter().map(|(_, bytes, _)| bytes).sum();
    kept.sort_by_key(|(idle, _, _)| std::cmp::Reverse(*idle));
    for (idle, bytes, snapshot) in kept {
        if holding <= held || idle < IN_USE {
            break;
        }
        if fs::remove_dir_all(&snapshot).is_ok() {
            holding -= bytes;
        }
    }
}

/// What the files in `dir` hold of the disk: a snapshot's memory is sparse where it is zero.
fn held_by(dir: &Path) -> u64 {
    fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| entry.metadata().ok())
        .map(|meta| {
            #[cfg(unix)]
            let bytes = std::os::unix::fs::MetadataExt::blocks(&meta) * 512;
            #[cfg(not(unix))]
            let bytes = meta.len();
            bytes
        })
        .sum()
}

// Off Unix a directory's time is not set through `File::open`, and there are no snapshots.
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::time::SystemTime;

    /// A snapshot of `bytes` at `dir`, of `rootfs`, last taken or restored from `idle` ago.
    fn plant(dir: PathBuf, rootfs: &Path, bytes: usize, idle: Duration) -> PathBuf {
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("memory"), vec![1u8; bytes]).unwrap();
        fs::write(dir.join("rootfs"), rootfs.to_str().unwrap()).unwrap();
        File::open(&dir)
            .unwrap()
            .set_modified(SystemTime::now() - idle)
            .unwrap();
        dir
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "virtx-uvm-snapshot-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    const HOUR: Duration = Duration::from_secs(60 * 60);

    #[test]
    fn a_sweep_removes_what_no_machine_here_can_restore_once_idle_and_what_none_has_for_a_day() {
        let home = scratch("sweep");
        let image = home.join("image.vmdk");
        fs::write(&image, "").unwrap();
        let gone = home.join("gone.vmdk");
        let ours = home.join("ours");
        let image_gone = plant(ours.join("1"), &gone, 1, 2 * HOUR);
        let image_gone_lately = plant(ours.join("2"), &gone, 1, Duration::ZERO);
        let half_written = plant(ours.join("3.taking-1"), &image, 1, 2 * HOUR);
        let image_there = plant(ours.join("4"), &image, 1, 2 * HOUR);
        let unused = plant(ours.join("5"), &image, 1, 25 * HOUR);
        let other_host = plant(home.join("theirs").join("1"), &image, 1, 2 * HOUR);
        let other_lately = plant(home.join("lately").join("1"), &image, 1, Duration::ZERO);

        sweep(&home, Some("ours"));

        assert!(!image_gone.exists());
        assert!(image_gone_lately.exists());
        assert!(!half_written.exists());
        assert!(image_there.exists());
        assert!(!unused.exists());
        assert!(!other_host.parent().unwrap().exists());
        assert!(other_lately.exists());
        fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn a_sweep_keeps_a_home_s_snapshots_within_what_they_may_hold_the_least_lately_used_going() {
        let home = scratch("held");
        let image = home.join("image.vmdk");
        fs::write(&image, "").unwrap();
        let ours = home.join("ours");
        let block = 64 << 10;
        let oldest = plant(ours.join("1"), &image, block, 3 * HOUR);
        let older = plant(ours.join("2"), &image, block, 2 * HOUR);
        let old = plant(ours.join("3"), &image, block, HOUR);
        // The most of all, but restored from a moment ago.
        let in_use = plant(ours.join("4"), &image, 4 * block, Duration::from_secs(1));

        sweep_to(&home, Some("ours"), held_by(&in_use) + held_by(&old));

        assert!(!oldest.exists());
        assert!(!older.exists());
        assert!(old.exists());
        assert!(in_use.exists());

        // Over again, with nothing left but what is in use and what is newer than all else.
        sweep_to(&home, Some("ours"), 0);
        assert!(!old.exists());
        assert!(in_use.exists());
        fs::remove_dir_all(&home).unwrap();
    }
}
