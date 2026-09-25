//! Put the guest kernel beside whatever is being built, so that a binary linking this
//! crate can boot a VM without being told where the kernel is.
//!
//! libkrun does not link `libkrunfw`. It resolves it at runtime by *leaf name* --
//! `LoadLibrary("libkrunfw.dll")`, `dlopen("libkrunfw.5.dylib")` -- and every platform
//! looks in the directory of the running binary before it looks anywhere else. So the
//! whole of what "installing the kernel" means is that the file sits next to the
//! executable, and the whole of what this does is put it there: `build/krunfw/` is where
//! `xtask/scripts/build-libkrunfw.sh` leaves it, and `target/<profile>/` is where cargo
//! leaves the binaries.
//!
//! **Copied rather than linked, and copied rather than pointed at.** A link argument
//! would be wrong twice over -- nothing links this library, and a build script's
//! `rustc-link-arg` reaches its own package's binaries rather than a dependent's. A
//! search path would be an environment variable to remember, which is what this is for
//! not needing.
//!
//! **It does not build one, and it fails if there is none.** `cargo xtask build kernel`
//! builds it, and that takes ten to twenty minutes in a container the first time -- not
//! something to start from a `cargo check` that a keystroke triggered, which is the same
//! line `docread/build.rs` draws around pdfium. What is left here is the cheap half, and
//! it is an error rather than a warning because a build of this crate that quietly
//! produced a binary unable to boot anything is worth less than a build that stops and
//! says which command is missing.

use std::path::{Path, PathBuf};

fn main() {
    println!("cargo::rerun-if-changed=build.rs");

    let root = match manifest_dir().parent() {
        Some(root) => root.to_path_buf(),
        None => return,
    };
    let from = root.join("build/krunfw");
    println!("cargo::rerun-if-changed={}", from.display());

    let Some(kernel) = library(&from) else {
        panic!(
            "no libkrunfw in {} -- run `cargo xtask build kernel`",
            from.display()
        );
    };
    let Some(profile) = profile_dir() else {
        return;
    };
    let name = kernel.file_name().expect("a file has a name");
    // Three, because cargo puts a binary, an example and a test in three different
    // directories, and "beside the running binary" is the only thing the loader looks at.
    for to in [
        profile.clone(),
        profile.join("examples"),
        profile.join("deps"),
    ] {
        if let Err(e) =
            std::fs::create_dir_all(&to).and_then(|()| std::fs::copy(&kernel, to.join(name)))
        {
            println!(
                "cargo::warning=copying {} into {} failed: {e}",
                kernel.display(),
                to.display()
            );
        }
    }
}

/// The kernel in `dir`, whatever this platform calls it.
///
/// The Linux name carries the full version (`libkrunfw.so.5.6.1`), so this matches rather
/// than names.
fn library(dir: &Path) -> Option<PathBuf> {
    let matches = |name: &str| {
        if cfg!(windows) {
            name == "libkrunfw.dll"
        } else if cfg!(target_os = "macos") {
            name.starts_with("libkrunfw.") && name.ends_with(".dylib")
        } else {
            name.starts_with("libkrunfw.so")
        }
    };
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(matches)
        })
}

fn manifest_dir() -> PathBuf {
    PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("cargo sets this"))
}

/// `target/<profile>/`, which cargo names nowhere directly.
///
/// `OUT_DIR` is `target/<profile>/build/<package>-<hash>/out`, so the profile directory is
/// three above it. Read from there rather than assembled out of `PROFILE` and
/// `CARGO_TARGET_DIR`, which get it wrong for a custom target directory, for a `--target`
/// triple, and for any profile that is not one of the two built-in names.
fn profile_dir() -> Option<PathBuf> {
    let out = PathBuf::from(std::env::var_os("OUT_DIR")?);
    Some(out.ancestors().nth(3)?.to_path_buf())
}
