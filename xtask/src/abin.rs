//! `/abin`: the programs every guest carries, cross-compiled for its musl target from
//! `virtx-abins/`. Apart from the rest of xtask because CI keeps what this builds by what it is
//! built from -- `virtx-abins/`, the toolchain, the musl scripts and this file -- so a change to
//! the rest of xtask builds nothing again.

use std::{
    collections::HashMap,
    env, fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::{Context, bail, ensure};

use crate::{cargo, guest_target, place, place_dir, root_path, target_path};

/// Each program's package in `virtx-abins/`, and the name it has in the guest.
const ABINS: &[(&str, &str)] = &[
    ("virtx-exec-index", "index"),
    ("virtx-exec-docread", "docread"),
    ("virtx-exec-display", "display"),
];

/// Build the `/abin` programs for the guest, in the debug or release profile, and place them,
/// their `SKILL.md` and what each installs in `out`.
pub(crate) fn build(debug: bool, out: &Path) -> anyhow::Result<()> {
    let root = root_path();
    let target = guest_target()?;
    let arch = target.split('-').next().unwrap_or_default();
    let sysroot = root.join(format!("build/musl-sysroot/{arch}/usr/include/stdio.h"));
    if !sysroot.exists() {
        let status = Command::new("sh")
            .arg(root.join("xtask/scripts/fetch-musl-sysroot.sh"))
            .status()
            .context("running fetch-musl-sysroot.sh")?;
        ensure!(status.success(), "fetching the musl sysroot failed");
        ensure!(sysroot.exists(), "no musl sysroot at {}", sysroot.display());
    }

    // What `musl-cc.sh` and `musl-ar.sh` run, asked for here rather than failing deep in
    // cc-rs's output. Not where `CC_<triple>` names a compiler of the caller's own, which
    // `virtx-abins/.cargo/config.toml` leaves to win.
    let own_cc = env::var_os(format!("CC_{}", target.replace('-', "_"))).is_some();
    let llvm_ar = [
        "llvm-ar",
        "/opt/homebrew/opt/llvm/bin/llvm-ar",
        "/usr/local/opt/llvm/bin/llvm-ar",
    ];
    let on_path = |name: &str| Command::new(name).arg("--version").output().is_ok();
    if !own_cc && (!Path::new("/usr/bin/clang").is_file() || !llvm_ar.iter().any(|ar| on_path(ar)))
    {
        bail!(
            "building abin needs /usr/bin/clang and llvm-ar -- {}",
            if cfg!(target_os = "macos") {
                "xcode-select --install, and brew install lld llvm"
            } else {
                "install the clang, lld and llvm packages (apt-get install clang lld llvm)"
            }
        );
    }

    let crate_dir = root.join("virtx-abins");
    let mut cargo = cross_cargo(&crate_dir);
    cargo.args(["build", "--target", target]);
    if !debug {
        cargo.arg("--release");
    }
    for (package, _) in ABINS {
        cargo.args(["-p", package]);
    }
    // The messages, for where each package's build script wrote: what one puts in
    // `OUT_DIR/install` is what its program installs beside itself.
    cargo
        .arg("--message-format=json-render-diagnostics")
        .stdout(Stdio::piped());
    let built = cargo
        .output()
        .with_context(|| format!("running cargo for abin for {target}"))?;
    ensure!(built.status.success(), "building abin for {target} failed");
    let mut out_dirs = HashMap::new();
    for line in String::from_utf8_lossy(&built.stdout).lines() {
        let Ok(message) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if message["reason"] != "build-script-executed" {
            continue;
        }
        // `path+file:///…/virtx-abins/docread#virtx-exec-docread@0.1.0`
        let package = message["package_id"].as_str().and_then(|id| id.rsplit_once('#'));
        let package = package.and_then(|(_, name)| name.split('@').next());
        if let (Some(package), Some(dir)) = (package, message["out_dir"].as_str()) {
            out_dirs.insert(package.to_owned(), Path::new(dir).join("install"));
        }
    }

    let profile = if debug { "debug" } else { "release" };
    fs::create_dir_all(out).with_context(|| format!("creating {}", out.display()))?;
    let built = target_path().join(format!("{target}/{profile}"));
    let mut installed = HashMap::new();
    for (package, program) in ABINS {
        place(&built.join(program), &out.join(program))?;

        // Under `install/<program>/`, at the paths it has in `/abin`; replaced whole, so that
        // what a program stopped installing goes.
        let at = out.join("install").join(program);
        if at.exists() {
            fs::remove_dir_all(&at).with_context(|| format!("removing {}", at.display()))?;
        }
        if let Some(from) = out_dirs.get(*package).filter(|dir| dir.is_dir()) {
            for file in files(from)? {
                let path = file.strip_prefix(from)?.to_path_buf();
                if let Some(other) = installed.insert(path.clone(), *program) {
                    bail!("{program} and {other} both install {}", path.display());
                }
            }
            place_dir(from, &at)?;
        }

        let skill = crate_dir.join(program).join("SKILL.md");
        let at = out.join("skills").join(program);
        if skill.is_file() {
            fs::create_dir_all(&at).with_context(|| format!("creating {}", at.display()))?;
            place(&skill, &at.join("SKILL.md"))?;
        } else {
            let _ = fs::remove_file(at.join("SKILL.md"));
            let _ = fs::remove_dir(&at);
        }
    }

    Ok(())
}

/// Each file under `dir`.
fn files(dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            found.extend(files(&entry.path())?);
        } else {
            found.push(entry.path());
        }
    }
    Ok(found)
}

fn cross_cargo(crate_dir: &Path) -> Command {
    let mut cargo = cargo();
    cargo
        .current_dir(crate_dir)
        .env("CARGO_TARGET_DIR", target_path())
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env_remove("CARGO_BUILD_TARGET")
        .env_remove("RUSTC_WRAPPER")
        .env_remove("RUSTC_WORKSPACE_WRAPPER");
    cargo
}
