//! Pull an OCI image and run a command in it, on a writable layer of its own.
//!
//!   cargo run --example run
//!   cargo run --example run -- alpine:3.21 'uname -srm'
//!
//! The first run pulls the image and stitches it into a disk; later ones find both in
//! the cache. The session's writes go to `data/sessions/<image>.ext4`, so a second run
//! sees what the first one did:
//!
//!   cargo run --example run -- alpine:3.21 'echo hello > /root/note'
//!   cargo run --example run -- alpine:3.21 'cat /root/note'
//!
//! Deleting that file is how the session starts over.
//!
//! A third argument is a host directory the guest sees at `/mnt`, read-only:
//!
//!   cargo run --example run -- alpine:3.21 'ls /mnt' ./scripts
//!
//! `CORTEX_GPU=1` asks for guest Vulkan, which needs the `gpu` feature to be anything
//! more than a flag and an image carrying Mesa's venus driver:
//!
//!   CORTEX_GPU=1 cargo run --features gpu --example run -- alpine:3.21 'vulkaninfo --summary'
//!
//! Unlike the `macos_*` and `win_*` examples, this one is not gated by host. Nothing in
//! it is host-specific: [`session`](cortex_krun::session) attaches the guest's console
//! itself, which is the one call that differs between them. The cargo runner still is,
//! though, and both are registered in `.cargo/config.toml`: on macOS
//! `scripts/macos-run.sh` ad-hoc signs the binary with
//! `com.apple.security.hypervisor`, which Hypervisor.framework will not start a VM
//! without, and on Windows `scripts/windows-run.ps1` puts `libkrunfw.dll` on `PATH`.

use std::path::PathBuf;

use cortex_krun::image;
use cortex_krun::session::{Ext4, Mount, Session};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let reference = args.next().unwrap_or_else(|| "alpine:latest".to_string());
    let command = args.next();
    let share = args.next();

    // The pull is the only async thing here, and the only thing that touches the
    // network. A machine is entered on this thread, so the runtime is over before
    // anything of libkrun's starts.
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(image::pull(reference.as_str(), None, None))?;
    drop(runtime);

    // One session per image, kept between runs -- named after the image with the
    // characters a path cannot hold replaced, since a reference holds `/` and `:`.
    let named: String = reference
        .chars()
        .map(
            |c| match c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                true => c,
                false => '_',
            },
        )
        .collect();
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("data/sessions")
        .join(format!("{named}.ext4"));

    eprintln!("session: {}", path.display());

    // Formatted here if this is the first run over that path, and found as it was left if
    // it is not -- which is what makes a later run carry on from this one. `data/sessions`
    // is made on the way. A session handed no rootfs would get a throwaway instead,
    // unlinked as soon as the machine had it.
    let rootfs = Ext4::at(path, Ext4::SIZE)?;

    // What to run: a shell around whatever was asked for, and the image's own command
    // when nothing was.
    let mut session = Session::new().image(reference.as_str()).rootfs(rootfs);
    if let Some(script) = command {
        session = session.command(["/bin/sh".to_string(), "-c".to_string(), script]);
    }

    // Read-only, which the host enforces: the guest may read what is shared and the
    // virtiofs server refuses its writes. `/mnt` need not exist in the image -- the guest
    // makes it on the overlay.
    if let Some(dir) = share {
        eprintln!("mount: {dir} -> /mnt (read-only)");
        session = session.mount(Mount::new(dir, "/mnt").read_only());
    }

    // Ignored by a build without the `gpu` feature, which is what makes it safe to read
    // an environment variable rather than gate the example on the feature.
    if std::env::var_os("CORTEX_GPU").is_some_and(|on| on == "1") {
        session = session.gpu(true);
    }

    // Does not return: the VMM exits this process with the guest's status.
    session.enter()?;
    unreachable!("a session that was entered does not come back")
}
