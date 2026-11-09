//! Build the agent that runs inside each machine, `virtx-uvm-guest`, for the guest -- Linux on
//! this host's ISA, linked statically against musl -- so `machine::boot` can embed it.

use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let guest = manifest.join("../virtx-uvm-guest");
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap();
    let target = format!("{arch}-unknown-linux-musl");

    println!("cargo:rerun-if-changed={}", guest.join("src").display());
    println!(
        "cargo:rerun-if-changed={}",
        guest.join("Cargo.toml").display()
    );
    println!("cargo:rerun-if-changed=src/machine/wire.rs");

    // From the guest's directory, so its `.cargo/config.toml` picks the linker. Apart from
    // this build's target directory, whose lock the cargo running this script holds; and
    // without the flags it hands build scripts, which would override that config's.
    let target_dir = out.join("guest-target");
    let status = Command::new(env::var("CARGO").unwrap())
        .current_dir(&guest)
        .args(["build", "--release", "--target", &target, "--target-dir"])
        .arg(&target_dir)
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env_remove("RUSTFLAGS")
        .status()
        .expect("running cargo for virtx-uvm-guest");
    assert!(
        status.success(),
        "building virtx-uvm-guest for {target} (if the target is missing: rustup target add {target})"
    );
    std::fs::copy(
        target_dir.join(&target).join("release/virtx-uvm-guest"),
        out.join("virtx-uvm-guest"),
    )
    .expect("copying virtx-uvm-guest");
}
