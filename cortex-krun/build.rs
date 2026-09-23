//! Cross-compile the guest and leave it beside this binary.
//!
//! The boot needs nothing from here: `cortex-krun-host` is a member of this workspace, so
//! cargo links it into the same directory as this binary on its own. The guest cannot be,
//! because it is a different platform -- a static musl ELF for the linux inside the VM, where
//! this half is whatever machine you are sitting at. One `cargo build` produces one target.
//!
//! So it is built here by a cargo of its own, from its own directory: it is a workspace root,
//! which is what keeps a bare `cargo build` from compiling it for darwin, and being invoked
//! there is what lets its `.cargo/config.toml` name a linker that can produce an ELF.
//!
//! It lands in `target/<profile>/`, the directory `OUT_DIR` sits three levels under, so that
//! a `cargo build` of this crate produces both files and the server can find the guest by
//! looking next to itself. See `src/session/halves.rs` for the other end of that.
//!
//! # Beside rather than inside
//!
//! It could be `include_bytes!`d instead, which would make this one file. What that costs is
//! a copy of it on the way to every boot and a server binary carrying an executable it never
//! runs itself. Two files that ship together is the cheaper arrangement: a machine's share
//! gets the agent by copying a file rather than by writing a few megabytes out of this
//! binary's own image.
//!
//! # What this needs installed
//!
//! The musl target, and nothing else:
//!
//! ```sh
//! rustup target add aarch64-unknown-linux-musl   # or x86_64-…, matching the host
//! ```
//!
//! A build without it fails here with that line in the message, rather than later with a
//! guest that will not start. `CORTEX_KRUN_GUEST_BIN` names a file to place instead of
//! building one, for a caller who builds the guest some other way.

use std::{
    path::{Path, PathBuf},
    process::Command,
};

fn main() -> anyhow::Result<()> {
    println!("cargo::rerun-if-changed=build.rs");
    guest(&beside()?)
}

/// Where cargo is putting this crate's binaries: `OUT_DIR` is
/// `<target>/<profile>/build/<pkg>-<hash>/out`, so the profile directory is three up.
///
/// Derived rather than asked for, because cargo tells a build script where its own output
/// goes and nothing else -- there is no variable naming the directory the linked binary lands
/// in, and reconstructing it from `CARGO_TARGET_DIR`, the profile name and a `--target` that
/// may or may not have been passed is the same answer with more ways to be wrong.
fn beside() -> anyhow::Result<PathBuf> {
    let out = PathBuf::from(std::env::var("OUT_DIR")?);
    out.ancestors()
        .nth(3)
        .map(Path::to_path_buf)
        .ok_or_else(|| {
            anyhow::anyhow!("OUT_DIR {} is not inside a target directory", out.display())
        })
}

fn guest(beside: &Path) -> anyhow::Result<()> {
    let out = beside.join("cortex-krun-guest");

    // A binary already built, for anyone who would rather not wait for a nested build on
    // every change to this half.
    println!("cargo::rerun-if-env-changed=CORTEX_KRUN_GUEST_BIN");
    if let Some(prebuilt) = std::env::var_os("CORTEX_KRUN_GUEST_BIN") {
        let prebuilt = PathBuf::from(prebuilt);
        println!("cargo::rerun-if-changed={}", prebuilt.display());
        return place(&prebuilt, &out);
    }

    let crate_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?)
        .join("../cortex-krun-guest")
        .canonicalize()?;
    for source in ["src", "Cargo.toml", ".cargo/config.toml"] {
        println!(
            "cargo::rerun-if-changed={}",
            crate_dir.join(source).display()
        );
    }
    // The guest links cortex and the boot's library half, so a change to either is a change
    // to the guest. Cargo would rebuild them inside the nested build; what this buys is that
    // the nested build is run at all.
    println!("cargo::rerun-if-changed=../cortex-krun-host/src/lib.rs");
    println!("cargo::rerun-if-changed=../../cortex/cortex/src");

    // A libkrun guest runs the host's architecture: there is no emulation in the middle to
    // make anything else work.
    let target = match std::env::consts::ARCH {
        "aarch64" => "aarch64-unknown-linux-musl",
        "x86_64" => "x86_64-unknown-linux-musl",
        other => anyhow::bail!("no guest target for a {other} host"),
    };
    let target_dir = PathBuf::from(std::env::var("OUT_DIR")?).join("guest");

    // `--release` because this file is copied into a machine's share on the way to every
    // boot, so its size is paid more than once.
    let status = cargo(&crate_dir, &target_dir)
        .args(["build", "--release", "--target", target])
        .status()
        .map_err(|e| anyhow::anyhow!("running cargo in {}: {e}", crate_dir.display()))?;
    anyhow::ensure!(
        status.success(),
        "building cortex-krun-guest for {target} failed.\n\
         If the target is not installed: rustup target add {target}"
    );

    place(
        &target_dir.join(format!("{target}/release/cortex-krun-guest")),
        &out,
    )
}

/// Put `built` at `out`.
///
/// Through a temporary name in the same directory and a rename, for two reasons. A rename is
/// atomic, so a server looking beside itself never finds a half-written file; and it replaces
/// the *name* rather than the bytes, so a machine that is already running its own copy is not
/// disturbed by a new build landing on it.
fn place(built: &Path, out: &Path) -> anyhow::Result<()> {
    let part = out.with_extension(format!("{}.part", std::process::id()));
    std::fs::copy(built, &part)
        .map_err(|e| anyhow::anyhow!("copying {} to {}: {e}", built.display(), part.display()))?;
    std::fs::rename(&part, out)
        .map_err(|e| anyhow::anyhow!("renaming {} to {}: {e}", part.display(), out.display()))
}

/// A cargo to run in `crate_dir`, with a target directory of its own and none of this build's
/// choices -- those were made for this build's target.
///
/// A shared target directory would deadlock on cargo's own lock, and an inherited `RUSTFLAGS`
/// or `RUSTC_WRAPPER` would be applied to a target it was not chosen for.
fn cargo(crate_dir: &Path, target_dir: &Path) -> Command {
    let mut cargo = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()));
    // From the crate's own directory, so its `.cargo/config.toml` -- where it names the
    // linker that can produce an ELF here -- is the configuration in effect.
    cargo
        .current_dir(crate_dir)
        .env("CARGO_TARGET_DIR", target_dir)
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env_remove("CARGO_BUILD_TARGET")
        .env_remove("RUSTC_WRAPPER")
        .env_remove("RUSTC_WORKSPACE_WRAPPER");
    cargo
}
