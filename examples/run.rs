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
//! Unlike the `macos_*` and `win_*` examples, this one is not gated by host. Nothing in
//! it is host-specific: [`session`](cortex_krun::session) attaches the guest's console
//! itself, which is the one call that differs between them. The cargo runner still is,
//! though, and both are registered in `.cargo/config.toml`: on macOS
//! `scripts/macos-run.sh` ad-hoc signs the binary with
//! `com.apple.security.hypervisor`, which Hypervisor.framework will not start a VM
//! without, and on Windows `scripts/windows-run.ps1` puts `libkrunfw.dll` on `PATH`.

use std::path::PathBuf;

use cortex_krun::image;
use cortex_krun::session::{Session, enter};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let reference = args.next().unwrap_or_else(|| "alpine:latest".to_string());
    let command = args.next();

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
    let upper_layer = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("data/sessions")
        .join(format!("{named}.ext4"));

    eprintln!("session: {}", upper_layer.display());

    // What to run: a shell around whatever was asked for, and the image's own command
    // when nothing was.
    let command = match command {
        Some(script) => vec!["/bin/sh".to_string(), "-c".to_string(), script],
        None => Vec::new(),
    };

    // Does not return: the VMM exits this process with the guest's status.
    enter(
        reference.as_str(),
        &Session {
            command,
            ..Session::at(upper_layer)
        },
        None,
    )?;
    unreachable!("a session that was entered does not come back")
}
