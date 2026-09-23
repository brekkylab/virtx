//! Finding and opening the virtio-console port the console session runs over.

use std::fs::File;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use cortex_krun_host::PORT_NAME;

/// How long to wait for the port to appear. The device is probed while the init above this
/// is still mounting, so it is normally there already; the wait is for the boot where it is
/// not yet.
const TIMEOUT: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(20);

/// Open the port, waiting for it to appear.
///
/// Read-write, and opened exactly once. The virtio-console driver allows a single opener per
/// port and answers a second with `EBUSY`, which is why the two directions this binary needs
/// come from `dup`ing this handle rather than from opening the device again.
pub fn open() -> anyhow::Result<File> {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if let Some(path) = find() {
            return File::options()
                .read(true)
                .write(true)
                .open(&path)
                .map_err(|e| anyhow::anyhow!("opening the console port {}: {e}", path.display()));
        }
        if Instant::now() >= deadline {
            anyhow::bail!(
                "no virtio-console port named `{PORT_NAME}` appeared -- the boot attached \
                 none, or the guest kernel has no virtio-console driver"
            );
        }
        std::thread::sleep(POLL);
    }
}

/// The device node for [`PORT_NAME`], if it is there yet.
///
/// Two ways to find it, because only one of them needs a udev the guest may not have: the
/// `/dev/virtio-ports/<name>` symlink is what a full system's rules create, and the `name`
/// attribute under `/sys/class/virtio-ports` is what those rules read. Falling back to the
/// second is what makes a minimal base image work.
fn find() -> Option<PathBuf> {
    let by_name = PathBuf::from("/dev/virtio-ports").join(PORT_NAME);
    if by_name.exists() {
        return Some(by_name);
    }

    // Each entry is a port; its `name` attribute is what the host tagged it with, and the
    // entry's own name (`vport0p1`) is the device node under `/dev`.
    for entry in std::fs::read_dir("/sys/class/virtio-ports").ok()?.flatten() {
        let named = std::fs::read_to_string(entry.path().join("name")).unwrap_or_default();
        if named.trim() != PORT_NAME {
            continue;
        }
        let node = PathBuf::from("/dev").join(entry.file_name());
        if node.exists() {
            return Some(node);
        }
    }
    None
}
