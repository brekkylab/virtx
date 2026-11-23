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
    ("virtx-exec-browser", "browser"),
];

/// Build the `/abin` programs for the guest, in the debug or release profile, and place them,
/// their `SKILL.md` and, as `.<program>/`, what each one's build script leaves in
/// `OUT_DIR/install/`, in `out`.
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
    cargo.args([
        "build",
        "--target",
        target,
        "--message-format=json-render-diagnostics",
    ]);
    if !debug {
        cargo.arg("--release");
    }
    for (package, _) in ABINS {
        cargo.args(["-p", package]);
    }
    let output = cargo
        .stderr(Stdio::inherit())
        .output()
        .with_context(|| format!("running cargo for abin for {target}"))?;
    ensure!(output.status.success(), "building abin for {target} failed");

    // Each program's `OUT_DIR`, as cargo names it for the build script it ran or found fresh. A
    // path package's id is `path+file://<its directory>#...`, and each one's directory is
    // `virtx-abins/<program>`.
    let mut out_dirs = HashMap::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let Ok(message) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if message["reason"] != "build-script-executed" {
            continue;
        }
        let (Some(id), Some(dir)) = (message["package_id"].as_str(), message["out_dir"].as_str())
        else {
            continue;
        };
        let Some(package) = id
            .strip_prefix("path+file://")
            .and_then(|id| id.split('#').next())
        else {
            continue;
        };
        if let Some((_, program)) = ABINS
            .iter()
            .find(|(_, program)| package.ends_with(&format!("/{program}")))
        {
            out_dirs.insert(*program, PathBuf::from(dir));
        }
    }

    let profile = if debug { "debug" } else { "release" };
    fs::create_dir_all(out).with_context(|| format!("creating {}", out.display()))?;
    let built = target_path().join(format!("{target}/{profile}"));
    for (_, program) in ABINS {
        place(&built.join(program), &out.join(program))?;

        // Replaced whole, so that nothing an earlier build placed is left in it.
        let at = out.join(format!(".{program}"));
        if at.is_dir() {
            fs::remove_dir_all(&at).with_context(|| format!("removing {}", at.display()))?;
        }
        let install = out_dirs.get(program).map(|dir| dir.join("install"));
        if let Some(install) = install.filter(|install| install.is_dir()) {
            place_dir(&install, &at)?;
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
