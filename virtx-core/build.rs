//! Writes `include/virtx.h`, the C declarations under `src/`, as `cbindgen.toml` sets.

use std::{env, path::PathBuf};

fn main() {
    // Without the C ABI there is nothing to declare.
    if env::var_os("CARGO_FEATURE_FFI").is_none() {
        return;
    }
    let crate_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    // A packaged crate ships the header as written, and its source is not written to.
    if crate_dir.join(".cargo_vcs_info.json").exists() {
        return;
    }
    let config = cbindgen::Config::from_file(crate_dir.join("cbindgen.toml"))
        .expect("reading cbindgen.toml");
    cbindgen::generate_with_config(&crate_dir, config)
        .expect("generating include/virtx.h")
        .write_to_file(crate_dir.join("include/virtx.h"));

    println!("cargo::rerun-if-changed=cbindgen.toml");
    println!("cargo::rerun-if-changed=src");
}
