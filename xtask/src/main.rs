use std::{
    env,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, bail, ensure};

const USAGE: &str = "\
usage: cargo xtask init     [--force]
       cargo xtask build    <host|guest|abin|kernel>... [--gpu] [--debug]
       cargo xtask download <guest|abin|kernel>... [--debug]
       cargo xtask install  [--prefix <dir>] [--debug]
       cargo xtask upload   [--prefix <dir>]
       cargo xtask run      <cargo run args>... [-- <program args>...]
       cargo xtask test     [<cargo test args>...] [-- <test harness args>...]

  init      initializes the repository, ready to build
  build     builds the components from source
  download  downloads the components prebuilt
  install   installs what was built or downloaded into <prefix>/bin
  upload    uploads what install put in <prefix>/bin, for download to fetch
  run       cargo run, with what a binary needs to boot a VM on this machine
  test      cargo test, the same way

  host      the console server and the VM process, for this machine
  guest     the program that runs inside the VM
  abin      the executables every VM gets at /abin
  kernel    the guest kernel, as libkrunfw -- see xtask/scripts/build-libkrunfw.sh

  --force         (init) resets each patched submodule to its pinned commit first,
                  discarding any local changes in it
  --prefix <dir>  where to install, or upload from (default: $CARGO_INSTALL_ROOT, $CARGO_HOME, ~/.cargo)
  --gpu           builds host with GPU support, and the GPU libraries it links first
  --debug         builds unoptimized (the guest is always optimized)";

const HOST_BINS: &[&str] = &["cortex-krun", "cortex-krun-host"];
const GUEST_BIN: &str = "cortex-krun-guest";
const ABINS: &[(&str, &str)] = &[("cortex-exec-index", "index"), ("cortex-exec-mem", "mem")];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Component {
    Host,
    Guest,
    Abin,
    Kernel,
}

struct Options {
    components: Vec<Component>,
    prefix: Option<PathBuf>,
    gpu: bool,
    debug: bool,
}

impl Options {
    fn has(&self, component: Component) -> bool {
        self.components.contains(&component)
    }

    fn profile(&self) -> &'static str {
        if self.debug { "debug" } else { "release" }
    }

    fn out(&self) -> PathBuf {
        target_path().join(self.profile())
    }
}

fn main() -> anyhow::Result<()> {
    let mut args = env::args_os().skip(1);
    let task = args.next();
    match task.as_deref().and_then(|a| a.to_str()) {
        Some("init") => {
            let force = match args.next() {
                None => false,
                Some(arg) if arg == "--force" => true,
                Some(arg) => bail!("unknown argument {}\n\n{USAGE}", arg.to_string_lossy()),
            };
            ensure!(args.next().is_none(), "init takes only --force\n\n{USAGE}");
            init(force)
        }
        Some("build") => {
            let options = parse(args)?;
            ensure!(
                options.prefix.is_none(),
                "--prefix is for install\n\n{USAGE}"
            );
            ensure!(
                !options.components.is_empty(),
                "name what to build\n\n{USAGE}"
            );
            ensure!(
                !options.gpu || options.has(Component::Host),
                "--gpu is for building host\n\n{USAGE}"
            );
            build(&options)
        }
        Some("download") => {
            let options = parse(args)?;
            ensure!(
                options.prefix.is_none(),
                "--prefix is for install\n\n{USAGE}"
            );
            ensure!(
                !options.components.is_empty(),
                "name what to download\n\n{USAGE}"
            );
            ensure!(!options.gpu, "--gpu is for building host\n\n{USAGE}");
            ensure!(
                !options.has(Component::Host),
                "host is built and signed on this machine, not downloaded\n\n{USAGE}"
            );
            download(&options)
        }
        Some("install") => {
            let options = parse(args)?;
            ensure!(
                options.components.is_empty() && !options.gpu,
                "install copies what build and download left; it takes only --prefix and --debug\n\n{USAGE}"
            );
            install(&options)
        }
        Some("upload") => {
            let options = parse(args)?;
            ensure!(
                options.components.is_empty() && !options.gpu && !options.debug,
                "upload takes only --prefix\n\n{USAGE}"
            );
            upload(&options)
        }
        Some("run") => run_target(args.collect(), false),
        Some("test") => run_target(args.collect(), true),
        Some("-h" | "--help") | None => {
            println!("{USAGE}");
            Ok(())
        }
        Some(other) => bail!("unknown task `{other}`\n\n{USAGE}"),
    }
}

fn parse(mut args: impl Iterator<Item = OsString>) -> anyhow::Result<Options> {
    let mut options = Options {
        components: Vec::new(),
        prefix: None,
        gpu: false,
        debug: false,
    };
    while let Some(arg) = args.next() {
        let component = match arg.to_str() {
            Some("host") => Component::Host,
            Some("guest") => Component::Guest,
            Some("abin") => Component::Abin,
            Some("kernel") => Component::Kernel,
            Some("--prefix") => {
                options.prefix = Some(args.next().context("--prefix needs a directory")?.into());
                continue;
            }
            Some("--gpu") => {
                options.gpu = true;
                continue;
            }
            Some("--debug") => {
                options.debug = true;
                continue;
            }
            _ => bail!("unknown argument {}\n\n{USAGE}", arg.to_string_lossy()),
        };
        if !options.has(component) {
            options.components.push(component);
        }
    }
    Ok(options)
}

fn root_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask is inside the workspace")
        .to_path_buf()
}

fn target_path() -> PathBuf {
    env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| root_path().join("target"))
}

fn guest_target() -> anyhow::Result<&'static str> {
    match env::consts::ARCH {
        "aarch64" => Ok("aarch64-unknown-linux-musl"),
        "x86_64" => Ok("x86_64-unknown-linux-musl"),
        other => bail!("no guest target for a {other} host"),
    }
}

fn cargo() -> Command {
    Command::new(env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
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

fn run(mut command: Command, what: &str) -> anyhow::Result<()> {
    let status = command
        .status()
        .with_context(|| format!("running cargo for {what}"))?;
    ensure!(status.success(), "building {what} failed");
    Ok(())
}

fn init(force: bool) -> anyhow::Result<()> {
    let root = root_path();
    let patches = root.join("3rdparty/_patches");
    let mut files: Vec<PathBuf> = fs::read_dir(&patches)
        .with_context(|| format!("reading {}", patches.display()))?
        .map(|entry| entry.map(|e| e.path()))
        .collect::<Result<_, _>>()?;
    files.retain(|path| path.extension().is_some_and(|ext| ext == "patch"));
    files.sort();

    eprintln!("Applying patches");
    let mut reset: Vec<String> = Vec::new();
    for patch in &files {
        let name = patch.file_name().unwrap().to_string_lossy();
        let submodule = name
            .match_indices('-')
            .find_map(|(at, _)| {
                let rest = &name[at + 1..];
                let commit = &rest[..rest.find('-')?];
                ((7..=40).contains(&commit.len()) && commit.bytes().all(|b| b.is_ascii_hexdigit()))
                    .then(|| &name[..at])
            })
            .with_context(|| format!("{name} is not <submodule>-<commit>-<what>.patch"))?;
        let dir = root.join("3rdparty").join(submodule);
        if force && !reset.iter().any(|done| done == submodule) {
            eprintln!("  {submodule}: resetting to its pinned commit");
            let status = Command::new("git")
                .arg("-C")
                .arg(&root)
                .args(["submodule", "update", "--init", "--force", "--"])
                .arg(format!("3rdparty/{submodule}"))
                .status()
                .context("running git")?;
            ensure!(status.success(), "{submodule}: git submodule update failed");
            let status = Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(["clean", "-fdq"])
                .status()
                .context("running git")?;
            ensure!(status.success(), "{submodule}: git clean failed");
            reset.push(submodule.to_string());
        }
        ensure!(
            dir.join(".git").exists(),
            "{submodule} is not checked out; run: git submodule update --init"
        );
        let git = |args: &[&str]| -> anyhow::Result<bool> {
            Ok(Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(args)
                .arg(patch)
                .stderr(std::process::Stdio::null())
                .status()
                .context("running git")?
                .success())
        };

        if git(&["apply", "--reverse", "--check"])? {
            eprintln!("  {submodule}: already patched");
            continue;
        }
        ensure!(
            git(&["apply", "--check"])?,
            "{submodule}: patch does not apply -- the submodule has moved or has other local \
             changes. `cargo xtask init --force` resets it, or refresh {}.",
            patch.display()
        );
        ensure!(git(&["apply"])?, "{submodule}: git apply failed");
        eprintln!("  {submodule}: applied {name}");
    }
    Ok(())
}

fn build(options: &Options) -> anyhow::Result<()> {
    if options.has(Component::Host) {
        build_host(options)?;
    }
    if options.has(Component::Guest) {
        build_guest(options)?;
    }
    if options.has(Component::Abin) {
        build_abin(options)?;
    }
    if options.has(Component::Kernel) {
        build_kernel()?;
    }
    eprintln!("xtask: built into {}", options.out().display());
    Ok(())
}

fn build_host(options: &Options) -> anyhow::Result<()> {
    if options.gpu {
        let scripts = root_path().join("xtask/scripts");
        let mut deps = if cfg!(windows) {
            let mut ps = Command::new("powershell");
            ps.args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
                .arg(scripts.join("build-deps.ps1"));
            ps
        } else {
            let mut sh = Command::new("sh");
            sh.arg(scripts.join("build-deps.sh"));
            sh
        };
        let status = deps.status().context("running build-deps")?;
        ensure!(status.success(), "building the GPU libraries failed");
    }
    let mut cargo = cargo();
    cargo.current_dir(root_path()).arg("build");
    for bin in HOST_BINS {
        cargo.args(["-p", bin]);
    }
    if options.gpu {
        cargo.args(["--features", "cortex-krun-host/gpu"]);
    }
    if !options.debug {
        cargo.arg("--release");
    }
    run(cargo, "host")?;

    sign(&options.out().join(format!("cortex-krun-host{}", env::consts::EXE_SUFFIX)))
}

fn sign(exe: &Path) -> anyhow::Result<()> {
    if !cfg!(target_os = "macos") {
        return Ok(());
    }
    let out = Command::new("codesign")
        .arg("--entitlements")
        .arg(root_path().join("xtask/hvf-entitlements.plist"))
        .args(["--force", "-s", "-"])
        .arg(exe)
        .output()
        .context("running codesign")?;
    ensure!(
        out.status.success(),
        "codesign of {} failed -- without the hypervisor entitlement it cannot create a VM\n{}",
        exe.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(())
}

fn runtime_env(command: &mut Command) -> anyhow::Result<()> {
    if cfg!(target_os = "macos") {
        let brew = Command::new("brew").arg("--prefix").output();
        if let Ok(brew) = brew
            && brew.status.success()
        {
            let mut fallback = OsString::from(String::from_utf8_lossy(&brew.stdout).trim());
            fallback.push("/lib:");
            match env::var_os("DYLD_FALLBACK_LIBRARY_PATH") {
                Some(existing) => fallback.push(existing),
                None => {
                    let home = env::var_os("HOME").unwrap_or_default();
                    fallback.push(home);
                    fallback.push("/lib:/usr/local/lib:/usr/lib");
                }
            }
            command.env("DYLD_FALLBACK_LIBRARY_PATH", fallback);
        }
    } else if cfg!(windows) {
        let root = root_path();
        let krunfw = root.join("build/krunfw");
        ensure!(
            krunfw.join("libkrunfw.dll").is_file(),
            "no libkrunfw.dll in {}; run xtask/scripts/build-libkrunfw.sh first",
            krunfw.display()
        );
        let mut dirs = vec![krunfw];
        let epoxy = root.join("build/epoxy/bin");
        if epoxy.is_dir() {
            dirs.push(epoxy);
        }
        dirs.extend(env::split_paths(&env::var_os("PATH").unwrap_or_default()));
        command.env("PATH", env::join_paths(dirs)?);
    }
    Ok(())
}

fn run_target(args: Vec<OsString>, test: bool) -> anyhow::Result<()> {
    let split = args.iter().position(|a| a == "--");
    let (cargo_args, program_args) = match split {
        Some(at) => (&args[..at], &args[at + 1..]),
        None => (&args[..], &[][..]),
    };

    let mut cargo = cargo();
    if test {
        cargo.args(["test", "--no-run"]);
    } else {
        cargo.arg("build");
    }
    let out = cargo
        .args(cargo_args)
        .arg("--message-format=json-render-diagnostics")
        .stderr(std::process::Stdio::inherit())
        .output()
        .context("running cargo")?;
    ensure!(out.status.success(), "cargo {} failed", if test { "test" } else { "build" });

    let mut targets: Vec<(PathBuf, PathBuf)> = Vec::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let Ok(message) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if message["reason"] != "compiler-artifact" {
            continue;
        }
        let Some(exe) = message["executable"].as_str() else {
            continue;
        };
        let is_test = message["profile"]["test"].as_bool().unwrap_or(false);
        let kinds = message["target"]["kind"].as_array().cloned().unwrap_or_default();
        let runnable = kinds.iter().any(|k| k == "bin" || k == "example");
        if (test && is_test) || (!test && !is_test && runnable) {
            let manifest_dir = message["manifest_path"]
                .as_str()
                .and_then(|m| Path::new(m).parent())
                .map(Path::to_path_buf)
                .unwrap_or_else(root_path);
            targets.push((exe.into(), manifest_dir));
        }
    }

    if !test {
        ensure!(
            targets.len() == 1,
            "cargo built {} runnable targets; name one with --bin, --example or -p",
            targets.len()
        );
    }

    let mut failed = 0;
    for (exe, manifest_dir) in &targets {
        sign(exe)?;
        let mut command = Command::new(exe);
        command.args(program_args);
        if test {
            eprintln!("     Running {}", exe.display());
            command
                .current_dir(manifest_dir)
                .env("CARGO_MANIFEST_DIR", manifest_dir);
        }
        runtime_env(&mut command)?;
        let status = command
            .status()
            .with_context(|| format!("running {}", exe.display()))?;
        if !test {
            std::process::exit(status.code().unwrap_or(1));
        }
        if !status.success() {
            failed += 1;
        }
    }
    ensure!(failed == 0, "{failed} test binaries failed");
    Ok(())
}

fn build_guest(options: &Options) -> anyhow::Result<()> {
    let target = guest_target()?;
    let mut cargo = cross_cargo(&root_path().join("cortex-krun-guest"));
    cargo.args(["build", "--release", "--target", target]);
    run(
        cargo,
        &format!("guest for {target} (if the target is missing: rustup target add {target})"),
    )?;

    let out = options.out();
    fs::create_dir_all(&out).with_context(|| format!("creating {}", out.display()))?;
    place(
        &target_path().join(format!("{target}/release/{GUEST_BIN}")),
        &out.join(GUEST_BIN),
    )
}

fn build_abin(options: &Options) -> anyhow::Result<()> {
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

    let crate_dir = root.join("cortex-abins");
    let mut cargo = cross_cargo(&crate_dir);
    cargo.args(["build", "--target", target]);
    if !options.debug {
        cargo.arg("--release");
    }
    for (package, _) in ABINS {
        cargo.args(["-p", package]);
    }
    run(cargo, &format!("abin for {target}"))?;

    let out = options.out().join("abin");
    fs::create_dir_all(&out).with_context(|| format!("creating {}", out.display()))?;
    for (_, program) in ABINS {
        place(
            &target_path().join(format!("{target}/{}/{program}", options.profile())),
            &out.join(program),
        )?;

        let skill = crate_dir.join(program).join("SKILLS.md");
        let at = out.join("skills").join(program);
        if skill.is_file() {
            fs::create_dir_all(&at).with_context(|| format!("creating {}", at.display()))?;
            place(&skill, &at.join("SKILLS.md"))?;
        } else {
            let _ = fs::remove_file(at.join("SKILLS.md"));
            let _ = fs::remove_dir(&at);
        }
    }
    Ok(())
}


/// Where the guest kernel is built to, and what it is called there.
///
/// One directory under `build/`, like everything else this repository compiles, and the
/// name is the platform's: `libkrunfw.dll`, `libkrunfw.5.dylib`, `libkrunfw.so.5.6.1`.
/// The Linux name carries the full version, so this answers with whatever is there
/// rather than with a name it worked out.
fn kernel_library() -> anyhow::Result<PathBuf> {
    let dir = root_path().join("build/krunfw");
    let matches = |name: &str| {
        if cfg!(windows) {
            name == "libkrunfw.dll"
        } else if cfg!(target_os = "macos") {
            name.starts_with("libkrunfw.") && name.ends_with(".dylib")
        } else {
            name.starts_with("libkrunfw.so")
        }
    };
    let found = fs::read_dir(&dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| path.file_name().and_then(|n| n.to_str()).is_some_and(matches));
    found.with_context(|| {
        format!(
            "no libkrunfw in {} -- run `cargo xtask build kernel` first",
            dir.display()
        )
    })
}

/// Build the guest kernel.
///
/// The whole of it is in [`build-libkrunfw.sh`](../scripts/build-libkrunfw.sh), including
/// why this repository builds one at all rather than installing a release: the config
/// libkrunfw ships for x86_64 leaves the guest with no write-combining, which nothing
/// reports and which costs a factor of 36 on the path a GPU's weights take.
///
/// `sh` on every host, Windows included: the script is one piece of logic for three
/// platforms, and Git Bash is already required for the submodules.
fn build_kernel() -> anyhow::Result<()> {
    let mut sh = Command::new("sh");
    sh.arg(root_path().join("xtask/scripts/build-libkrunfw.sh"));
    let status = sh.status().context("running build-libkrunfw.sh")?;
    ensure!(status.success(), "building the guest kernel failed");
    Ok(())
}
fn download(options: &Options) -> anyhow::Result<()> {
    if options.has(Component::Guest) {
        todo!("download guest");
    }
    if options.has(Component::Abin) {
        todo!("download abin");
    }
    if options.has(Component::Kernel) {
        todo!("download kernel");
    }
    Ok(())
}

fn upload(options: &Options) -> anyhow::Result<()> {
    let prefix = match &options.prefix {
        Some(prefix) => prefix.clone(),
        None => default_prefix()?,
    };
    let _bin = prefix.join("bin");
    todo!("upload")
}

fn install(options: &Options) -> anyhow::Result<()> {
    let built = options.out();
    let prefix = match &options.prefix {
        Some(prefix) => prefix.clone(),
        None => default_prefix()?,
    };
    let bin = prefix.join("bin");

    let host_exes: Vec<String> = HOST_BINS
        .iter()
        .map(|name| format!("{name}{}", env::consts::EXE_SUFFIX))
        .collect();
    for exe in &host_exes {
        ensure!(
            built.join(exe).is_file(),
            "no {exe} in {} -- run `cargo xtask build host` first",
            built.display()
        );
    }
    ensure!(
        built.join(GUEST_BIN).is_file(),
        "no {GUEST_BIN} in {} -- run `cargo xtask build guest` or `cargo xtask download guest` first",
        built.display()
    );

    // The guest kernel goes with them. libkrun resolves it by leaf name, and every
    // platform looks beside the running binary before it looks anywhere else -- so a
    // binary installed next to it needs no PATH, no DYLD_FALLBACK_LIBRARY_PATH and no
    // wrapper to be run by hand.
    let kernel = kernel_library()?;

    fs::create_dir_all(&bin).with_context(|| format!("creating {}", bin.display()))?;
    place(&kernel, &bin.join(kernel.file_name().expect("the kernel is a file")))?;
    for exe in &host_exes {
        place(&built.join(exe), &bin.join(exe))?;
    }
    place(&built.join(GUEST_BIN), &bin.join(GUEST_BIN))?;
    if built.join("abin").is_dir() {
        place_dir(&built.join("abin"), &bin.join("abin"))?;
    }

    eprintln!("xtask: installed to {}", bin.display());
    Ok(())
}

fn default_prefix() -> anyhow::Result<PathBuf> {
    if let Some(root) = env::var_os("CARGO_INSTALL_ROOT") {
        return Ok(root.into());
    }
    if let Some(home) = env::var_os("CARGO_HOME") {
        return Ok(home.into());
    }
    let home = env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .context("no home directory to put ~/.cargo in; pass --prefix")?;
    Ok(PathBuf::from(home).join(".cargo"))
}

fn place(from: &Path, to: &Path) -> anyhow::Result<()> {
    ensure!(from.is_file(), "{} was not built", from.display());
    let part = to.with_extension(format!("{}.part", std::process::id()));
    fs::copy(from, &part)
        .with_context(|| format!("copying {} to {}", from.display(), part.display()))?;
    fs::rename(&part, to)
        .with_context(|| format!("renaming {} to {}", part.display(), to.display()))
}

fn place_dir(from: &Path, to: &Path) -> anyhow::Result<()> {
    fs::create_dir_all(to).with_context(|| format!("creating {}", to.display()))?;
    for entry in fs::read_dir(from).with_context(|| format!("reading {}", from.display()))? {
        let entry = entry?;
        let path = entry.path();
        let dest = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            place_dir(&path, &dest)?;
        } else {
            place(&path, &dest)?;
        }
    }
    Ok(())
}
