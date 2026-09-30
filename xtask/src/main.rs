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
       cargo xtask build    <host|guest|abin|kernel>... [--debug]
       cargo xtask download <guest|abin|kernel>... [--debug]
       cargo xtask install  [--prefix <dir>] [--debug]
       cargo xtask upload   --bucket <name> [--profile <name>] [--region <region>]
                            [--latest] [--allow-dirty] [--dry-run]
       cargo xtask run      <cargo run args>... [-- <program args>...]
       cargo xtask test     [<cargo test args>...] [-- <test harness args>...]

  init      initializes the repository, ready to build
  build     builds the components from source
  download  downloads the components prebuilt
  install   installs what was built or downloaded into <prefix>/bin
  upload    archives what install would put in <prefix>/bin, for this platform, and
            publishes it to S3 for cortex's ensure_cortex to fetch
  run       cargo run, with what a binary needs to boot a VM on this machine
  test      cargo test, the same way

  host      the console server and the VM process, for this machine
  guest     the program that runs inside the VM
  abin      the executables every VM gets at /abin
  kernel    the guest kernel, as libkrunfw -- see xtask/scripts/build-libkrunfw.sh

  --force         (init) resets each patched submodule to its pinned commit first,
                  discarding any local changes in it
  --prefix <dir>  where to install (default: $CORTEX_HOME, else cortex under
                  ~/Library/Caches on macOS, %LOCALAPPDATA% on Windows, $XDG_CACHE_HOME or
                  ~/.cache otherwise)
  --debug         builds unoptimized (the guest is always optimized)

  --bucket <name>     (upload) the bucket to publish to (default: $CORTEX_DIST_BUCKET)
  --profile <name>    (upload) the aws CLI profile (default: the CLI's own -- $AWS_PROFILE,
                      the environment's keys, or `default`)
  --region <region>   (upload) the bucket's region (default: the profile's)
  --latest            (upload) also puts the archive under `latest`, which is what
                      `ensure_cortex` fetches when told no version; without it `latest`
                      holds whatever it held before
  --allow-dirty       (upload) publishes from a tree with uncommitted changes, filed under
                      `<sha>-dirty` rather than the sha
  --dry-run           (upload) archives, says what it would upload, and stops";

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
            ensure!(
                !options.has(Component::Host),
                "host is built and signed on this machine, not downloaded\n\n{USAGE}"
            );
            download(&options)
        }
        Some("install") => {
            let options = parse(args)?;
            ensure!(
                options.components.is_empty(),
                "install copies what build and download left; it takes only --prefix and --debug\n\n{USAGE}"
            );
            install(&options)
        }
        Some("upload") => upload(&parse_upload(args)?),
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

/// Where `upload` publishes to, and how.
///
/// Its own flags rather than [`Options`]: nothing here names a component or a profile, and
/// none of it means anything to another task.
struct Upload {
    bucket: String,
    profile: Option<String>,
    region: Option<String>,
    /// Whether to move this platform's `latest` to the release, and only when asked
    /// (`--latest`): an upload adds a sha anyone can fetch by name, and harms nobody; moving
    /// `latest` changes what every `ensure_cortex` told no version fetches next.
    latest: bool,
    allow_dirty: bool,
    dry_run: bool,
}

fn parse_upload(mut args: impl Iterator<Item = OsString>) -> anyhow::Result<Upload> {
    let mut bucket = env::var("CORTEX_DIST_BUCKET")
        .ok()
        .filter(|b| !b.is_empty());
    let mut upload = Upload {
        bucket: String::new(),
        profile: None,
        region: None,
        latest: false,
        allow_dirty: false,
        dry_run: false,
    };
    fn value(flag: &str, args: &mut impl Iterator<Item = OsString>) -> anyhow::Result<String> {
        args.next()
            .and_then(|v| v.into_string().ok())
            .with_context(|| format!("{flag} needs a value"))
    }
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--bucket") => bucket = Some(value("--bucket", &mut args)?),
            Some("--profile") => upload.profile = Some(value("--profile", &mut args)?),
            Some("--region") => upload.region = Some(value("--region", &mut args)?),
            Some("--latest") => upload.latest = true,
            Some("--allow-dirty") => upload.allow_dirty = true,
            Some("--dry-run") => upload.dry_run = true,
            _ => bail!("unknown argument {}\n\n{USAGE}", arg.to_string_lossy()),
        }
    }
    upload.bucket = bucket.context("name a bucket with --bucket or $CORTEX_DIST_BUCKET")?;
    Ok(upload)
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

/// Always with the GPU. A host without a Vulkan loader still runs the build -- the boot opens
/// the loader at run time and reports `gpu` only where it finds one -- so one build serves
/// hosts with a GPU and without, and there is no second one to keep working.
fn build_host(options: &Options) -> anyhow::Result<()> {
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

    let mut cargo = cargo();
    cargo.current_dir(root_path()).arg("build");
    for bin in HOST_BINS {
        cargo.args(["-p", bin]);
    }
    cargo.args(["--features", "cortex-krun-host/gpu"]);
    if !options.debug {
        cargo.arg("--release");
    }
    run(cargo, "host")?;

    sign(
        &options
            .out()
            .join(format!("cortex-krun-host{}", env::consts::EXE_SUFFIX)),
    )
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
    ensure!(
        out.status.success(),
        "cargo {} failed",
        if test { "test" } else { "build" }
    );

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
        let kinds = message["target"]["kind"]
            .as_array()
            .cloned()
            .unwrap_or_default();
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
/// name is the platform's: `libkrunfw.dll`, `libkrunfw.5.dylib`, `libkrunfw.so.5` -- the
/// soname libkrun opens, which `build-libkrunfw.sh` renames the built file to. The Linux
/// name carries the ABI version, so this answers with whatever is there rather than with
/// a name it worked out.
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
        .find(|path| {
            path.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(matches)
        });
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
/// why this repository builds one at all rather than installing a release, and why never
/// one older than v5.6.2: before it, the x86_64 config left the guest with no
/// write-combining, which nothing reports and which costs a factor of 36 on the path a
/// GPU's weights take.
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

/// Where a release is filed in the bucket: `cortex-krun/<ref>/cortex-krun-<os>-<arch>.tar.gz`,
/// one archive per platform under a directory named for the release.
///
/// **One level for every name a release has.** `<ref>` is the sha of the commit it was built
/// from, which this writes; or `latest`, which `--latest` writes as well, and which
/// `ensure_cortex` fetches when told no version; or a tag, which the dist workflow writes
/// once every platform has its archive. A fetch is the same URL whichever name it is given,
/// and a name is a directory of archives rather than a file naming one, so nothing reads one
/// thing to find out where another is.
///
/// Every archive carries the sha it was built from as `x-amz-meta-sha`, so what a name holds
/// is a HEAD away.
const DIST_ROOT: &str = "cortex-krun";

/// The platforms a release is published for, as `std::env::consts` spells them -- which
/// is what `ensure_cortex` builds its URL from. macOS is Apple silicon only and Windows
/// x86_64 only for now.
fn dist_platform() -> anyhow::Result<String> {
    let (os, arch) = (env::consts::OS, env::consts::ARCH);
    ensure!(
        matches!(
            (os, arch),
            ("macos", "aarch64") | ("windows", "x86_64") | ("linux", "x86_64" | "aarch64")
        ),
        "no release is published for {os}-{arch}"
    );
    Ok(format!("{os}-{arch}"))
}

/// Archive the set `install` would put in `bin/` and publish it.
///
/// The git sha of HEAD names the release, as it does abin's, and nothing downstream checks
/// the bytes against it -- so a tree with uncommitted changes is refused unless asked for,
/// and then filed under `<sha>-dirty` so the path does not claim to be the commit. What it
/// cannot check is that the build is *of* HEAD: it archives whatever `build` left.
fn upload(upload: &Upload) -> anyhow::Result<()> {
    let platform = dist_platform()?;
    let root = root_path();

    let git = |args: &[&str]| -> anyhow::Result<String> {
        let out = Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(args)
            .output()
            .context("running git")?;
        ensure!(out.status.success(), "git {} failed", args.join(" "));
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    let sha = git(&["rev-parse", "HEAD"])?;
    // Untracked files are left out: an editor's swap file is not a change to the build.
    let dirty = !git(&["status", "--porcelain", "--untracked-files=no"])?.is_empty();
    ensure!(
        !dirty || upload.allow_dirty,
        "the working tree has uncommitted changes; commit them, or --allow-dirty"
    );
    let version = if dirty {
        format!("{sha}-dirty")
    } else {
        sha.clone()
    };
    if git(&["branch", "-r", "--contains", &sha])?.is_empty() {
        eprintln!("xtask: warning: {sha} is on no remote branch; nobody else can check it out");
    }

    // Staged under the release profile's own directory, and emptied first: a file a
    // previous build had and this one does not must not ride along.
    let dist = target_path().join("release").join("dist");
    let name = format!("{DIST_ROOT}-{platform}");
    let stage = dist.join(&name);
    if stage.exists() {
        fs::remove_dir_all(&stage).with_context(|| format!("emptying {}", stage.display()))?;
    }
    stage_bin(&target_path().join("release"), &stage)?;
    if !stage.join("abin").is_dir() {
        eprintln!("xtask: warning: no abin was built; sessions from this release get no /abin");
    }

    // Relative paths only, run from `dist`: Git Bash's GNU tar reads `C:` in a path as a
    // remote host, and Windows' own bsdtar has no `--force-local` to say otherwise.
    let archive = format!("{name}.tar.gz");
    let mut entries: Vec<OsString> = fs::read_dir(&stage)
        .with_context(|| format!("reading {}", stage.display()))?
        .map(|entry| entry.map(|e| e.file_name()))
        .collect::<Result<_, _>>()?;
    entries.sort();
    let status = Command::new("tar")
        .current_dir(&dist)
        // macOS' bsdtar otherwise adds an AppleDouble `._` file for any file with xattrs.
        .env("COPYFILE_DISABLE", "1")
        .arg("-czf")
        .arg(&archive)
        .arg("-C")
        .arg(&name)
        .args(&entries)
        .status()
        .context("running tar")?;
    ensure!(status.success(), "archiving {} failed", stage.display());

    let prefix = format!("s3://{}/{DIST_ROOT}", upload.bucket);
    let archive_key = format!("{prefix}/{version}/{archive}");
    let latest_key = format!("{prefix}/latest/{archive}");
    if upload.dry_run {
        eprintln!(
            "xtask: would upload {} to {archive_key}",
            dist.join(&archive).display()
        );
        if upload.latest {
            eprintln!("xtask: would upload it to {latest_key} too");
        } else {
            eprintln!("xtask: would leave {latest_key} as it is (--latest replaces it)");
        }
        return Ok(());
    }

    let aws = |from: &Path, to: &str, cache: &str| -> anyhow::Result<()> {
        let mut aws = Command::new("aws");
        aws.args(["s3", "cp", "--only-show-errors", "--cache-control", cache])
            .args(["--content-type", "application/gzip"])
            .args(["--metadata", &format!("sha={version}")]);
        if let Some(profile) = &upload.profile {
            aws.args(["--profile", profile]);
        }
        if let Some(region) = &upload.region {
            aws.args(["--region", region]);
        }
        let status = aws
            .arg(from)
            .arg(to)
            .status()
            .context("running the aws CLI -- is it installed?")?;
        ensure!(status.success(), "uploading {to} failed");
        Ok(())
    };
    // The archive under its sha never changes, so it may be cached forever; `latest` is a
    // name that moves, so it may not be, and is written second, so that the sha's archive is
    // there by the time it names it.
    aws(
        &dist.join(&archive),
        &archive_key,
        "public, max-age=31536000, immutable",
    )?;
    eprintln!("xtask: uploaded {archive_key}");
    if upload.latest {
        aws(&dist.join(&archive), &latest_key, "no-cache")?;
        eprintln!("xtask: {latest_key} is now {version}");
    } else {
        eprintln!("xtask: left {latest_key} as it is (--latest replaces it)");
    }
    Ok(())
}

fn install(options: &Options) -> anyhow::Result<()> {
    let prefix = match &options.prefix {
        Some(prefix) => prefix.clone(),
        None => cortex::cache_root(),
    };
    let bin = prefix.join("bin");
    stage_bin(&options.out(), &bin)?;
    eprintln!("xtask: installed to {}", bin.display());
    Ok(())
}

/// Put the set a `bin/` holds -- the host halves, the guest, the guest kernel, and abin if
/// it was built -- from `built` into `bin`. What `install` does, and what `upload` archives.
fn stage_bin(built: &Path, bin: &Path) -> anyhow::Result<()> {
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
    place(
        &kernel,
        &bin.join(kernel.file_name().expect("the kernel is a file")),
    )?;
    for exe in &host_exes {
        place(&built.join(exe), &bin.join(exe))?;
    }
    place(&built.join(GUEST_BIN), &bin.join(GUEST_BIN))?;
    if built.join("abin").is_dir() {
        place_dir(&built.join("abin"), &bin.join("abin"))?;
    }
    Ok(())
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
