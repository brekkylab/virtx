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
       cargo xtask build    <component>... [--debug] [--target <triple>] [--locked]
       cargo xtask install  [--prefix <dir>] [--debug]
       cargo xtask check    [--client]
       cargo xtask clippy   [--gpu]
       cargo xtask archive  [--with-abin]
       cargo xtask run      <cargo run args>... [-- <program args>...]
       cargo xtask test     [<cargo test args>... [-- <test harness args>...]]
       cargo xtask fixture  -- <command>...
       cargo xtask fixture  --serve

  init      checks out the submodules that are not, and adds the guest's musl target --
            ready to build the servers
  build     builds the components from source
  install   installs what was built into <prefix>/bin
  check     what a change has to pass before it is pushed: the format, clippy and the tests,
            of the client always and of the servers once `init` has run; and the bindings'
            lint -- ruff, biome and tsc -- where uv and npm are on PATH
  clippy    check's clippy alone: the client's, and the servers' once `init` has run. With
            --gpu, the servers' with the `gpu` feature a release builds them with, the GPU
            libraries built first as `build` builds them; for this host's own code, the
            `cfg`s a Linux check never sees
  archive   archives the stdio server and the guest kernel, for this platform, as a release
            carries them: target/release/dist/virtx-stdio-<os>-<arch>.tar.gz
  run       cargo run, with what a binary needs to boot a VM on this machine
  test      every test: the crates' own, then the packages' against the fixture, each
            suite summed up at the end. With arguments, cargo test, the way run is, against
            the fixture where they name virtx
  fixture   runs any <command> against the servers, built and signed, over a home of its own:
            VIRTX_TEST_STDIO names virtx-stdio for it to spawn, and VIRTX_TEST_URL,
            VIRTX_TEST_TOKEN and VIRTX_TEST_PROJECT virtx-server-tcp, served for it on the
            loopback; both and the home are gone once it ends, and its exit is this one's.
            Where the environment names them already, <command> runs as it is. With --serve,
            it writes each of those as NAME=value on stdout, then `ready`, and serves until
            its stdin closes, for a test runner to start it the way pytest's conftest does

components:
  client        libvirtx_core, the library the Python, Node and Rust packages load
  server-stdio  the server a client spawns, as the `virtx-stdio` binary; also the VM process
  server-tcp   the server over HTTP, as the `virtx-server-tcp` binary
  kernel        the guest kernel, as libkrunfw -- see xtask/scripts/build-libkrunfw.sh
  abin          the executables every VM gets at /abin

  --force         (init) resets every submodule to its pinned commit first, discarding
                  any local changes in it
  --prefix <dir>  where to install (default: $VIRTX_HOME, else virtx under
                  ~/Library/Caches on macOS, %LOCALAPPDATA% on Windows, $XDG_CACHE_HOME or
                  ~/.cache otherwise)
  --debug         builds unoptimized
  --target <triple>   (build) cross-compiles the client to <triple>; the servers and abin are
                      built for this host and its guest only
  --locked        (build) passes --locked to cargo
  --client        (check) checks the client only, even where the servers could be checked
  --gpu           (clippy) the servers with their `gpu` feature, the GPU libraries built first
  --with-abin     (archive) puts the abin that was built in the archive too; without it
                  the archive has no abin, and sessions from the release no /abin";

/// The client's packages, which `check` checks before `init` has run.
const CLIENT_PACKAGES: &[&str] = &["virtx-core", "virtx", "virtx-cli"];
/// The package the client library is built from.
const CLIENT_PACKAGE: &str = "virtx-core";
/// The packages that link libkrun, and `xtask`, which `check` checks once `init` has run.
const SERVER_PACKAGES: &[&str] = &[
    "virtx-uvm",
    "virtx-server-stdio",
    "virtx-server-tcp",
    "xtask",
];
/// Each server: its component, the package it is built from, and the binary it builds. The
/// stdio server's binary is `virtx-stdio`, which is what a release archive carries
/// and what a client spawns.
const SERVERS: &[(Component, &str, &str)] = &[
    (Component::ServerStdio, "virtx-server-stdio", "virtx-stdio"),
    (Component::ServerTcp, "virtx-server-tcp", "virtx-server-tcp"),
];
const ABINS: &[(&str, &str)] = &[("virtx-exec-index", "index"), ("virtx-exec-mem", "mem")];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Component {
    Client,
    ServerStdio,
    ServerTcp,
    Kernel,
    Abin,
}

struct Options {
    components: Vec<Component>,
    prefix: Option<PathBuf>,
    debug: bool,
    target: Option<String>,
    locked: bool,
}

impl Options {
    fn has(&self, component: Component) -> bool {
        self.components.contains(&component)
    }

    fn profile(&self) -> &'static str {
        if self.debug { "debug" } else { "release" }
    }

    /// Where cargo builds this profile for this host. A client cross-compiled with `--target`
    /// is under the triple's own directory instead, as cargo puts it.
    fn out(&self) -> PathBuf {
        target_path().join(self.profile())
    }

    fn servers(&self) -> impl Iterator<Item = &(Component, &'static str, &'static str)> {
        SERVERS
            .iter()
            .filter(|(component, _, _)| self.has(*component))
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
                options.target.is_none() || options.components == [Component::Client],
                "--target cross-compiles the client alone\n\n{USAGE}"
            );
            build(&options)
        }
        Some("install") => {
            let options = parse(args)?;
            ensure!(
                options.components.is_empty() && options.target.is_none(),
                "install copies what build left; it takes only --prefix and --debug\n\n{USAGE}"
            );
            install(&options)
        }
        Some("check") => {
            let client_only = match args.next() {
                None => false,
                Some(arg) if arg == "--client" => true,
                Some(arg) => bail!("unknown argument {}\n\n{USAGE}", arg.to_string_lossy()),
            };
            ensure!(
                args.next().is_none(),
                "check takes only --client\n\n{USAGE}"
            );
            check(client_only)
        }
        Some("clippy") => {
            let gpu = match args.next() {
                None => false,
                Some(arg) if arg == "--gpu" => true,
                Some(arg) => bail!("unknown argument {}\n\n{USAGE}", arg.to_string_lossy()),
            };
            ensure!(args.next().is_none(), "clippy takes only --gpu\n\n{USAGE}");
            clippy(gpu)
        }
        Some("archive") => {
            let abin = match args.next() {
                None => false,
                Some(arg) if arg == "--with-abin" => true,
                Some(arg) => bail!("unknown argument {}\n\n{USAGE}", arg.to_string_lossy()),
            };
            ensure!(
                args.next().is_none(),
                "archive takes only --with-abin\n\n{USAGE}"
            );
            archive(abin)
        }
        Some("run") => run_target(args.collect(), false, &[]),
        Some("test") => {
            let args: Vec<OsString> = args.collect();
            match args.as_slice() {
                [] => test_all(),
                // virtx-rs's tests are against the fixture too.
                _ if args.iter().any(|arg| arg == "virtx") => {
                    let fixture = Fixture::start()?;
                    run_target(args, true, &fixture.vars())
                }
                _ => run_target(args, true, &[]),
            }
        }
        Some("fixture") => {
            let rest: Vec<OsString> = args.collect();
            match rest.split_first() {
                Some((dashes, command)) if dashes == "--" && !command.is_empty() => {
                    fixture(command)
                }
                Some((serve, [])) if serve == "--serve" => serve_fixture(),
                _ => bail!("fixture takes -- and the command to run, or --serve\n\n{USAGE}"),
            }
        }
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
        target: None,
        locked: false,
    };
    while let Some(arg) = args.next() {
        let component = match arg.to_str() {
            Some("client") => Component::Client,
            Some("server-stdio") => Component::ServerStdio,
            Some("server-tcp") => Component::ServerTcp,
            Some("kernel") => Component::Kernel,
            Some("abin") => Component::Abin,
            Some("--prefix") => {
                options.prefix = Some(args.next().context("--prefix needs a directory")?.into());
                continue;
            }
            Some("--debug") => {
                options.debug = true;
                continue;
            }
            Some("--target") => {
                let target = args.next().and_then(|t| t.into_string().ok());
                options.target = Some(target.context("--target needs a triple")?);
                continue;
            }
            Some("--locked") => {
                options.locked = true;
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

/// The submodules' paths, as `.gitmodules` lists them.
fn submodules() -> anyhow::Result<Vec<String>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root_path())
        .args([
            "config",
            "--file",
            ".gitmodules",
            "--get-regexp",
            r"\.path$",
        ])
        .output()
        .context("running git")?;
    ensure!(out.status.success(), "reading .gitmodules failed");
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| line.split_whitespace().nth(1).map(str::to_string))
        .collect())
}

/// Whether rustup has the guest's target for the toolchain this repository pins. `None` where
/// there is no rustup to ask, and the build is left to say.
fn has_guest_target() -> anyhow::Result<Option<bool>> {
    let Ok(out) = Command::new("rustup")
        .current_dir(root_path())
        .args(["target", "list", "--installed"])
        .output()
    else {
        return Ok(None);
    };
    let target = guest_target()?;
    Ok(Some(
        out.status.success()
            && String::from_utf8_lossy(&out.stdout)
                .lines()
                .any(|t| t == target),
    ))
}

fn init(force: bool) -> anyhow::Result<()> {
    let root = root_path();

    // Those not checked out yet; with --force, every one, reset to its pinned commit.
    let submodules = submodules()?;
    let update: Vec<&String> = submodules
        .iter()
        .filter(|path| force || !root.join(path).join(".git").exists())
        .collect();
    if !update.is_empty() {
        let what = if force { "Resetting" } else { "Checking out" };
        let names: Vec<&str> = update.iter().map(|p| p.as_str()).collect();
        eprintln!("{what} {}", names.join(", "));
        let mut git = Command::new("git");
        git.arg("-C")
            .arg(&root)
            .args(["submodule", "update", "--init"]);
        if force {
            git.arg("--force");
        }
        let status = git.arg("--").args(&names).status().context("running git")?;
        ensure!(status.success(), "git submodule update failed");
        if force {
            for path in &names {
                let status = Command::new("git")
                    .arg("-C")
                    .arg(root.join(path))
                    .args(["clean", "-fdq"])
                    .status()
                    .context("running git")?;
                ensure!(status.success(), "{path}: git clean failed");
            }
        }
    }

    // The agent every machine runs is built for it, by `virtx-uvm`'s build script.
    let target = guest_target()?;
    if has_guest_target()? == Some(false) {
        eprintln!("Adding {target}");
        let status = Command::new("rustup")
            .current_dir(&root)
            .args(["target", "add", target])
            .status()
            .context("running rustup")?;
        ensure!(status.success(), "rustup target add {target} failed");
    }
    Ok(())
}

/// Why the servers cannot be built here yet, if they cannot: what `init` does is not done.
fn servers_unready() -> anyhow::Result<Option<String>> {
    let root = root_path();
    for path in submodules()? {
        if !root.join(&path).join(".git").exists() {
            return Ok(Some(format!("{path} is not checked out")));
        }
    }
    if has_guest_target()? == Some(false) {
        return Ok(Some(format!("rustup has no {}", guest_target()?)));
    }
    Ok(None)
}

/// The gate a change passes before it is pushed. The client's half needs nothing but the
/// toolchain; the servers' half needs `init`, and is skipped, saying so, until it has run. The
/// servers' own tests boot machines, which takes a kernel and a signed binary: they are
/// `cargo xtask test -p <server>`, run when a change reaches them.
fn check(client_only: bool) -> anyhow::Result<()> {
    cargo_in_root(&["fmt", "--check"], "the format")?;
    clippy_client()?;
    cargo_in_root(&["test", "-p", CLIENT_PACKAGE], "the tests of virtx-core")?;
    lint_bindings()?;

    if client_only {
        eprintln!("xtask: the servers left out (--client)");
        return Ok(());
    }
    if let Some(why) = servers_unready()? {
        eprintln!("xtask: the servers left out: {why} -- `cargo xtask init` readies them");
        return Ok(());
    }
    clippy_servers(false)?;
    cargo_in_root(
        &["test", "-p", "virtx-uvm", "--lib"],
        "the unit tests of virtx-uvm",
    )?;
    eprintln!("xtask: checked");
    Ok(())
}

/// `check`'s clippy, alone: the client's, and the servers' once `init` has run -- with `gpu`,
/// as a release builds them, the GPU libraries built first. Run on each host, it is the lint of
/// that host's own `cfg`s, which a check on Linux never compiles.
fn clippy(gpu: bool) -> anyhow::Result<()> {
    clippy_client()?;
    if let Some(why) = servers_unready()? {
        ensure!(!gpu, "--gpu, but {why} -- `cargo xtask init` readies it");
        eprintln!("xtask: the servers left out: {why} -- `cargo xtask init` readies them");
        return Ok(());
    }
    if gpu {
        build_gpu_deps()?;
    }
    clippy_servers(gpu)
}

fn clippy_client() -> anyhow::Result<()> {
    let mut clippy = vec!["clippy"];
    clippy.extend(CLIENT_PACKAGES.iter().flat_map(|p| ["-p", *p]));
    clippy.extend(["--all-targets", "--all-features", "--", "-D", "warnings"]);
    cargo_in_root(&clippy, "clippy of the client")
}

/// The servers' clippy; with `gpu`, each with its `gpu` feature -- the only feature they have,
/// and the one a release builds them with -- which takes the GPU libraries built.
fn clippy_servers(gpu: bool) -> anyhow::Result<()> {
    let features: Vec<String> = SERVER_PACKAGES
        .iter()
        .filter(|p| gpu && **p != "xtask")
        .map(|p| format!("{p}/gpu"))
        .collect();
    let features = features.join(",");
    let mut clippy = vec!["clippy"];
    clippy.extend(SERVER_PACKAGES.iter().flat_map(|p| ["-p", *p]));
    if gpu {
        clippy.extend(["--features", features.as_str()]);
    }
    clippy.extend(["--all-targets", "--", "-D", "warnings"]);
    let what = if gpu {
        "clippy of the servers, with gpu"
    } else {
        "clippy of the servers"
    };
    cargo_in_root(&clippy, what)
}

/// The Python and the JavaScript beside the crates: `virtx-py`, `virtx-node` and the packages'
/// scripts under `scripts/packages`. ruff lints and checks the format of the Python (`ruff.toml`);
/// biome the JavaScript (`biome.json`), handed the two directories it lints, since from the root
/// it would walk the submodules too and warn of MoltenVK's broken links; and tsc type-checks
/// `virtx-node/index.d.ts`, the declarations written by hand beside `index.js`. Each tool is the
/// version its lockfile pins: ruff `virtx-py`'s dev group, biome and tsc `virtx-node`'s
/// devDependencies.
///
/// Left out, saying so, where uv or npm is not on PATH -- except under CI, where that is a
/// runner without what it needs.
fn lint_bindings() -> anyhow::Result<()> {
    let npm = if cfg!(windows) { "npm.cmd" } else { "npm" };
    let missing: Vec<&str> = ["uv", npm]
        .into_iter()
        .filter(|tool| {
            Command::new(tool)
                .arg("--version")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_err()
        })
        .collect();
    if !missing.is_empty() {
        let why = format!("no {} on PATH", missing.join(" or "));
        ensure!(env::var_os("CI").is_none(), "the bindings' lint: {why}");
        eprintln!("xtask: the bindings' lint left out: {why}");
        return Ok(());
    }
    let tool = |program: &str, args: &[&str], what: &str| -> anyhow::Result<()> {
        eprintln!("xtask: {what}");
        let status = Command::new(program)
            .current_dir(root_path())
            .args(args)
            .status()
            .with_context(|| format!("running {program}"))?;
        ensure!(status.success(), "{what} failed");
        Ok(())
    };
    let ruff = [
        "run",
        "--project",
        "virtx-py",
        "--only-group",
        "dev",
        "ruff",
    ];
    let python = ["virtx-py", "scripts/packages"];
    tool(
        "uv",
        &[&ruff[..], &["check"], &python].concat(),
        "ruff of the Python",
    )?;
    tool(
        "uv",
        &[&ruff[..], &["format", "--check"], &python].concat(),
        "the format of the Python",
    )?;
    tool(
        npm,
        &[
            "install",
            "--prefix",
            "virtx-node",
            "--no-audit",
            "--no-fund",
            "--silent",
        ],
        "virtx-node's tools",
    )?;
    let exec = ["exec", "--prefix", "virtx-node", "--"];
    tool(
        npm,
        &[
            &exec[..],
            &[
                "biome",
                "ci",
                "--error-on-warnings",
                "virtx-node",
                "scripts/packages",
            ],
        ]
        .concat(),
        "biome of the JavaScript",
    )?;
    tool(
        npm,
        &[&exec[..], &["tsc", "-p", "virtx-node/tsconfig.json"]].concat(),
        "tsc of index.d.ts",
    )
}

/// `cargo <args>` at the root, said first as `what`, which it fails as.
fn cargo_in_root(args: &[&str], what: &str) -> anyhow::Result<()> {
    eprintln!("xtask: {what}");
    let mut command = cargo();
    command.current_dir(root_path()).args(args);
    let status = command.status().context("running cargo")?;
    ensure!(status.success(), "{what} failed");
    Ok(())
}

fn build(options: &Options) -> anyhow::Result<()> {
    if options.has(Component::Client) {
        build_client(options)?;
    }
    if options.servers().next().is_some() {
        build_servers(options)?;
    }
    if options.has(Component::Abin) {
        build_abin(options)?;
    }
    if options.has(Component::Kernel) {
        build_kernel()?;
    }
    let out = match &options.target {
        Some(target) => target_path().join(target).join(options.profile()),
        None => options.out(),
    };
    eprintln!("xtask: built into {}", out.display());
    Ok(())
}

/// The client library's file name on this host: `libvirtx_core.dylib`, `libvirtx_core.so`,
/// `virtx_core.dll`.
fn client_library() -> String {
    format!(
        "{}virtx_core{}",
        env::consts::DLL_PREFIX,
        env::consts::DLL_SUFFIX
    )
}

/// The library alone, with the features its default is: no kernel and nothing to sign.
fn build_client(options: &Options) -> anyhow::Result<()> {
    let mut cargo = cargo();
    cargo
        .current_dir(root_path())
        .args(["build", "-p", CLIENT_PACKAGE, "--lib"]);
    if let Some(target) = &options.target {
        cargo.args(["--target", target]);
    }
    if !options.debug {
        cargo.arg("--release");
    }
    if options.locked {
        cargo.arg("--locked");
    }
    run(cargo, "the client")
}

/// The GPU libraries the servers' `gpu` feature links, built under `build/` by this host's
/// script; each script builds only what is not built yet.
fn build_gpu_deps() -> anyhow::Result<()> {
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
    Ok(())
}

/// The servers named, in one cargo run, each signed. Always with the GPU. A host without a
/// Vulkan loader still runs the build -- the boot opens the loader at run time and reports
/// `gpu` only where it finds one -- so one build serves hosts with a GPU and without, and there
/// is no second one to keep working.
fn build_servers(options: &Options) -> anyhow::Result<()> {
    build_gpu_deps()?;

    let mut cargo = cargo();
    cargo.current_dir(root_path()).arg("build");
    for (_, package, _) in options.servers() {
        cargo.args(["-p", package]);
        cargo.args(["--features", &format!("{package}/gpu")]);
    }
    if !options.debug {
        cargo.arg("--release");
    }
    if options.locked {
        cargo.arg("--locked");
    }
    run(cargo, "the servers")?;

    for (_, _, binary) in options.servers() {
        ready(
            &options
                .out()
                .join(format!("{binary}{}", env::consts::EXE_SUFFIX)),
        )?;
    }
    Ok(())
}

/// Make the server binary `exe` one that boots: the guest kernel beside it, where libkrun
/// looks, and the entitlement on it.
fn ready(exe: &Path) -> anyhow::Result<()> {
    let kernel = kernel_library()?;
    let beside = exe
        .parent()
        .context("the server binary is in a directory")?
        .join(kernel.file_name().expect("the kernel is a file"));
    if !beside.is_file() {
        place(&kernel, &beside)?;
    }
    sign(exe)
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

/// Build with cargo what `args` names, and run it, or each of its tests if `test`, with `vars`
/// added to its environment.
fn run_target(args: Vec<OsString>, test: bool, vars: &[(&str, OsString)]) -> anyhow::Result<()> {
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
        // A test's binary spawns the package's own to be the VM, which needs the entitlement
        // and the kernel too.
        if test && !is_test && runnable {
            let is_server = SERVERS
                .iter()
                .any(|(_, _, binary)| message["target"]["name"] == *binary);
            if is_server {
                ready(Path::new(exe))?;
            } else {
                sign(Path::new(exe))?;
            }
        }
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
        // A test may boot machines of itself, as `virtx-uvm`'s do: it needs the kernel beside
        // it, wherever there is one to put there.
        if test && kernel_library().is_ok() {
            ready(exe)?;
        } else {
            sign(exe)?;
        }
        let mut command = Command::new(exe);
        command.args(program_args).envs(vars.iter().cloned());
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

    let crate_dir = root.join("virtx-abins");
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

/// The platforms a release is published for, as `std::env::consts` spells them, which is
/// how an archive's name spells them. macOS is Apple silicon only and Windows
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

/// Archive the set `install` would put in `bin/`, as a release carries it:
/// `target/release/dist/virtx-stdio-<os>-<arch>.tar.gz`, which the `release` workflow attaches to
/// the tag's GitHub release. It archives whatever `build` left, and checks nothing about
/// which commit that was.
fn archive(abin: bool) -> anyhow::Result<()> {
    let platform = dist_platform()?;

    // Staged under the release profile's own directory, and emptied first: a file a
    // previous build had and this one does not must not ride along.
    let dist = target_path().join("release").join("dist");
    let name = format!("virtx-stdio-{platform}");
    let stage = dist.join(&name);
    if stage.exists() {
        fs::remove_dir_all(&stage).with_context(|| format!("emptying {}", stage.display()))?;
    }
    let built = target_path().join("release");
    ensure!(
        !abin || built.join("abin").is_dir(),
        "--with-abin, but no abin in {} -- run `cargo xtask build abin` first",
        built.display()
    );
    let stdio = format!("{}{}", SERVERS[0].2, env::consts::EXE_SUFFIX);
    ensure!(
        built.join(&stdio).is_file(),
        "no {stdio} in {} -- run `cargo xtask build server-stdio` first",
        built.display()
    );
    stage_bin(&built, &stage, &[stdio], abin)?;
    if !abin {
        eprintln!(
            "xtask: leaving abin out; sessions from this release get no /abin (--with-abin puts it in)"
        );
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
    eprintln!("xtask: archived {}", dist.join(&archive).display());
    Ok(())
}

fn install(options: &Options) -> anyhow::Result<()> {
    let prefix = match &options.prefix {
        Some(prefix) => prefix.clone(),
        None => cache_root(),
    };
    let bin = prefix.join("bin");
    let built = options.out();
    let files: Vec<String> = std::iter::once(client_library())
        .chain(
            SERVERS
                .iter()
                .map(|(_, _, binary)| format!("{binary}{}", env::consts::EXE_SUFFIX)),
        )
        .filter(|file| built.join(file).is_file())
        .collect();
    ensure!(
        !files.is_empty(),
        "nothing built in {} -- run `cargo xtask build <component>` first",
        built.display()
    );
    stage_bin(&built, &bin, &files, true)?;
    eprintln!("xtask: installed to {}", bin.display());
    Ok(())
}

/// Put the set a `bin/` holds -- the `files` named, the guest kernel when a server is among
/// them, and abin if it was built and `abin` asks for it -- from `built` into `bin`. What
/// `install` does, and what `archive` archives.
fn stage_bin(built: &Path, bin: &Path, files: &[String], abin: bool) -> anyhow::Result<()> {
    // The guest kernel goes with a server. libkrun resolves it by leaf name, and every
    // platform looks beside the running binary before it looks anywhere else -- so a
    // binary installed next to it needs no PATH, no DYLD_FALLBACK_LIBRARY_PATH and no
    // wrapper to be run by hand.
    let server = SERVERS
        .iter()
        .any(|(_, _, binary)| files.contains(&format!("{binary}{}", env::consts::EXE_SUFFIX)));
    let kernel = if server {
        Some(kernel_library()?)
    } else {
        None
    };

    fs::create_dir_all(bin).with_context(|| format!("creating {}", bin.display()))?;
    if let Some(kernel) = kernel {
        place(
            &kernel,
            &bin.join(kernel.file_name().expect("the kernel is a file")),
        )?;
    }
    for file in files {
        place(&built.join(file), &bin.join(file))?;
    }
    if abin && built.join("abin").is_dir() {
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

/// Where virtx keeps what it keeps on this host: `$VIRTX_HOME`, or a `virtx` directory in the
/// platform's cache directory.
fn cache_root() -> std::path::PathBuf {
    if let Some(named) = std::env::var_os("VIRTX_HOME") {
        return std::path::PathBuf::from(named);
    }

    #[cfg(windows)]
    return std::env::var_os("LOCALAPPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::PathBuf::from(std::env::var_os("USERPROFILE").unwrap())
                .join("AppData")
                .join("Local")
        })
        .join("virtx");

    #[cfg(target_os = "macos")]
    return std::path::PathBuf::from(std::env::var_os("HOME").unwrap())
        .join("Library")
        .join("Caches")
        .join("virtx");

    #[cfg(not(any(windows, target_os = "macos")))]
    return std::env::var_os("XDG_CACHE_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::PathBuf::from(std::env::var_os("HOME").unwrap()).join(".cache")
        })
        .join("virtx");
}

/// The token the fixture's server over HTTP lets a request through with, made up and no one's,
/// and its SHA-256, which its config holds.
const FIXTURE_TOKEN: &str = "test-token";
const FIXTURE_TOKEN_SHA256: &str =
    "4c5dc9b7708905f77f5e5d16316b5dfb425e68cb326dcd55a860e90a7707031e";
/// The one project the fixture's server over HTTP lets a request reach.
const FIXTURE_PROJECT: &str = "test";

/// Run `command` against the fixture, and exit as it does.
fn fixture(command: &[OsString]) -> anyhow::Result<()> {
    // Under a fixture already, as `cargo xtask test` runs `npm test`, the command runs in it.
    let fixture = match env::var_os("VIRTX_TEST_URL") {
        Some(_) => None,
        None => Some(Fixture::start()?),
    };
    let mut run = Command::new(&command[0]);
    run.args(&command[1..]);
    if let Some(fixture) = &fixture {
        run.envs(fixture.vars());
    }
    let status = run
        .status()
        .with_context(|| format!("running {}", command[0].to_string_lossy()))?;
    drop(fixture);
    std::process::exit(status.code().unwrap_or(1));
}

/// Serve the fixture until stdin closes, having written what it names in the environment on
/// stdout as `NAME=value` lines, then `ready`.
fn serve_fixture() -> anyhow::Result<()> {
    use std::io::{Read as _, Write as _};
    let fixture = Fixture::start()?;
    let mut stdout = std::io::stdout().lock();
    for (name, value) in fixture.vars() {
        writeln!(stdout, "{name}={}", value.to_string_lossy())?;
    }
    writeln!(stdout, "ready")?;
    stdout.flush()?;
    drop(stdout);
    let _ = std::io::stdin().read_to_end(&mut Vec::new());
    Ok(())
}

/// Each package's tests, the commands run in its directory against the fixture: virtx-rs's with
/// cargo, virtx-py's with pytest, and virtx-node's with Node's own runner.
const PACKAGES: &[(&str, &str, &[&[&str]])] = &[
    ("virtx-rs", ".", &[&["cargo", "test", "-p", "virtx"]]),
    ("virtx-py", "virtx-py", &[&["uv", "run", "pytest"]]),
    (
        "virtx-node",
        "virtx-node",
        &[&["npm", "install"], &["npm", "test"]],
    ),
];

/// How a suite of `test_all` ended.
enum Outcome {
    Passed,
    Failed(String),
    Skipped(String),
}

/// Every suite: the crates' own tests, then the packages' against one fixture, each run whatever
/// came before, and how each ended. The servers' suites and the packages' are skipped until the
/// servers can be built here.
fn test_all() -> anyhow::Result<()> {
    let mut outcomes: Vec<(&str, Outcome)> = Vec::new();

    let unready = match servers_unready()? {
        Some(why) => Some(format!("{why} -- `cargo xtask init` readies them")),
        None if kernel_library().is_err() => {
            Some("no guest kernel -- `cargo xtask build kernel` builds it".to_owned())
        }
        None => None,
    };
    let crates = [
        "virtx-core",
        "virtx-uvm",
        "virtx-server-stdio",
        "virtx-server-tcp",
    ];
    for name in crates {
        let outcome = match &unready {
            Some(why) if name != CLIENT_PACKAGE => Outcome::Skipped(why.clone()),
            _ => match run_target(vec!["-p".into(), name.into()], true, &[]) {
                Ok(()) => Outcome::Passed,
                Err(e) => Outcome::Failed(format!("{e:#}")),
            },
        };
        outcomes.push((name, outcome));
    }

    let fixture = match &unready {
        Some(why) => Err(Outcome::Skipped(why.clone())),
        None => Fixture::start().map_err(|e| Outcome::Failed(format!("the fixture: {e:#}"))),
    };
    for (name, dir, commands) in PACKAGES {
        let outcome = match &fixture {
            Err(Outcome::Skipped(why)) => Outcome::Skipped(why.clone()),
            Err(Outcome::Failed(why)) => Outcome::Failed(why.clone()),
            Err(Outcome::Passed) => unreachable!("the fixture started"),
            Ok(fixture) => {
                let mut outcome = Outcome::Passed;
                for argv in *commands {
                    let ran = Command::new(argv[0])
                        .args(&argv[1..])
                        .envs(fixture.vars())
                        .current_dir(root_path().join(dir))
                        .status();
                    let why = match ran {
                        Ok(status) if status.success() => continue,
                        Ok(status) => format!("`{}` {status}", argv.join(" ")),
                        Err(e) => format!("`{}` did not run: {e}", argv.join(" ")),
                    };
                    outcome = Outcome::Failed(why);
                    break;
                }
                outcome
            }
        };
        outcomes.push((name, outcome));
    }
    drop(fixture);

    eprintln!();
    for (name, outcome) in &outcomes {
        match outcome {
            Outcome::Passed => eprintln!("xtask: {name:<20} ok"),
            Outcome::Failed(why) => eprintln!("xtask: {name:<20} FAILED: {why}"),
            Outcome::Skipped(why) => eprintln!("xtask: {name:<20} skipped: {why}"),
        }
    }
    let failures = outcomes
        .iter()
        .filter(|(_, outcome)| matches!(outcome, Outcome::Failed(_)))
        .count();
    ensure!(failures == 0, "{failures} suites failed");
    Ok(())
}

/// The servers, built and signed, for tests to run against: `virtx-stdio` for each to spawn,
/// and `virtx-server-tcp` served on the loopback, over a config and a home made for it under a
/// directory of their own. The server is stopped, and the directory removed, when this drops.
struct Fixture {
    process: std::process::Child,
    stdio: PathBuf,
    url: String,
    scratch: PathBuf,
}

impl Fixture {
    fn start() -> anyhow::Result<Fixture> {
        if let Some(why) = servers_unready()? {
            bail!("{why} -- `cargo xtask init` readies the servers");
        }
        kernel_library().context("no guest kernel -- `cargo xtask build kernel` builds it")?;
        let options = Options {
            components: SERVERS.iter().map(|(component, _, _)| *component).collect(),
            prefix: None,
            debug: false,
            target: None,
            locked: false,
        };
        build_servers(&options)?;
        let built = |name: &str| {
            options
                .out()
                .join(format!("{name}{}", env::consts::EXE_SUFFIX))
        };

        let scratch = env::temp_dir().join(format!("virtx-fixture-{}", std::process::id()));
        if scratch.exists() {
            fs::remove_dir_all(&scratch)
                .with_context(|| format!("emptying {}", scratch.display()))?;
        }
        let home = scratch.join("home");
        fs::create_dir_all(&home).with_context(|| format!("creating {}", home.display()))?;
        let config = serde_json::json!({
            "listen": "127.0.0.1:0",
            "users": { "test": { "token_sha256": FIXTURE_TOKEN_SHA256 } },
            "projects": { FIXTURE_PROJECT: { "members": ["test"] } },
        });
        let config_path = scratch.join("config.json");
        fs::write(&config_path, config.to_string())
            .with_context(|| format!("writing {}", config_path.display()))?;

        let tcp = built("virtx-server-tcp");
        let mut process = Command::new(&tcp)
            .arg("serve")
            .arg("--config")
            .arg(&config_path)
            .env("VIRTX_HOME", &home)
            .stderr(std::process::Stdio::piped())
            .spawn()
            .with_context(|| format!("starting {}", tcp.display()))?;
        // It says where it listens first; the rest, its machines' consoles among it, is read
        // only so that it never blocks.
        let stderr = process.stderr.take().expect("piped");
        let mut lines = std::io::BufRead::lines(std::io::BufReader::new(stderr));
        let mut fixture = Fixture {
            process,
            stdio: built("virtx-stdio"),
            url: String::new(),
            scratch,
        };
        let line = lines
            .next()
            .context("virtx-server-tcp ended before it served")?
            .context("reading what virtx-server-tcp said")?;
        fixture.url = line
            .strip_prefix("serving at ")
            .with_context(|| format!("virtx-server-tcp said {line:?}, not where it serves"))?
            .to_owned();
        std::thread::spawn(move || lines.for_each(drop));
        eprintln!(
            "xtask: the fixture is served at {}, over {}",
            fixture.url,
            home.display()
        );
        Ok(fixture)
    }

    /// What it names to a command in its environment: the home both servers keep, where
    /// `virtx-stdio` is, and how to reach `virtx-server-tcp`.
    fn vars(&self) -> Vec<(&'static str, OsString)> {
        vec![
            ("VIRTX_HOME", self.scratch.join("home").into()),
            ("VIRTX_TEST_STDIO", self.stdio.clone().into()),
            ("VIRTX_TEST_URL", self.url.clone().into()),
            ("VIRTX_TEST_TOKEN", FIXTURE_TOKEN.into()),
            ("VIRTX_TEST_PROJECT", FIXTURE_PROJECT.into()),
        ]
    }
}

impl Drop for Fixture {
    /// Stop it as `ctrl-c` would, so that it ends its sandboxes' machines, and kill it if it
    /// has not ended a while later.
    fn drop(&mut self) {
        #[cfg(unix)]
        let _ = Command::new("kill")
            .args(["-INT", &self.process.id().to_string()])
            .status();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while matches!(self.process.try_wait(), Ok(None)) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let _ = self.process.kill();
        let _ = self.process.wait();
        let _ = fs::remove_dir_all(&self.scratch);
    }
}
