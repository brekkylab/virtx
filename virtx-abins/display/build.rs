//! The X server `display` runs: `xserver/build.sh` builds it from source, and this places it
//! in `libexec/display/` beside the program, where `/abin` has it too.
//!
//! The build runs in an Alpine virtx machine rather than on this host: Alpine has the static
//! musl archives it links, and the machine has the guest's architecture whatever this host is.
//! The client library is `$VIRTX_CORE_LIB`, and the server is `$VIRTX_STDIO` or else the one
//! of the library's version, fetched; `cargo xtask build abin` sets both to what it built.
//!
//! Only for a musl target, which is what `/abin` is built for: a build for this host, an
//! editor's say, builds the program alone. Cargo runs this again only when `xserver/` changes.

use std::{env, fs, path::Path};

use anyhow::{Context, bail, ensure};
use virtx::{
    Client, Files as _, ImageJobEntry, SandboxCreateArgs, SandboxExecArgs, SandboxJobEntry, Volume,
};

/// The image `build.sh` runs in.
const BASE: &str = "alpine:3.22";
/// What the build writes that runs; a volume's files come back without their modes.
const EXECUTABLES: &[&str] = &["Xvfb", "xkbcomp"];

fn main() -> anyhow::Result<()> {
    println!("cargo::rerun-if-changed=xserver");
    if env::var("CARGO_CFG_TARGET_ENV")? != "musl" {
        return Ok(());
    }
    let arch = env::var("CARGO_CFG_TARGET_ARCH")?;
    ensure!(
        arch == env::consts::ARCH,
        "the X server is built in a machine of this host's architecture, {}, not {arch}",
        env::consts::ARCH
    );
    // `OUT_DIR` is `<profile>/build/<package>-<hash>/out`, and the program goes in `<profile>/`.
    let out_dir = env::var_os("OUT_DIR").context("cargo sets OUT_DIR")?;
    let out = Path::new(&out_dir)
        .ancestors()
        .nth(3)
        .context("OUT_DIR is three directories into the profile's")?
        .join("libexec/display");

    let client = Client::spawn_virtx([] as [&str; 0]).context("starting a virtx server")?;
    let mut pull = client.pull_image(BASE, None).log();
    while let Some(entry) = pull.wait_next() {
        if let ImageJobEntry::End(end) = entry?.entry
            && let Some(error) = end.error
        {
            bail!("pulling {BASE}: {error}");
        }
    }

    let volume = client.create_volume(None::<String>).wait()?;
    let built = (|| {
        for name in ["build.sh", "sources"] {
            let data = fs::read(Path::new("xserver").join(name))
                .with_context(|| format!("reading xserver/{name}"))?;
            volume.write_file(name, data, None).wait()?;
        }
        let vcpus = std::thread::available_parallelism().map_or(2, |n| n.get().min(8)) as u8;
        let sandbox = client
            .create_sandbox(
                &SandboxCreateArgs::new(BASE)
                    .vcpus(vcpus)
                    .memory_mib(4096)
                    .volume(volume.name(), "/build"),
            )
            .wait()?;
        // On the machine's own disk, and copied to the volume once it is whole.
        let mut log = sandbox
            .exec(&SandboxExecArgs::new([
                "sh",
                "-c",
                "sh /build/build.sh /tmp/out && cp -R /tmp/out /build/out",
            ]))
            .log();
        let mut code = None;
        while let Some(entry) = log.wait_next() {
            match entry?.entry {
                SandboxJobEntry::Stdout(line) | SandboxJobEntry::Stderr(line) => {
                    eprintln!("{line}")
                }
                SandboxJobEntry::End(end) => code = Some(end.code),
                _ => {}
            }
        }
        let _ = sandbox.remove().wait();
        ensure!(code == Some(0), "build.sh failed (exit {code:?})");

        if out.exists() {
            fs::remove_dir_all(&out).with_context(|| format!("removing {}", out.display()))?;
        }
        fetch(&volume, "out/", &out)?;
        for name in EXECUTABLES {
            let path = out.join(name);
            ensure!(path.is_file(), "the build wrote no {}", path.display());
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
                    .with_context(|| format!("making {} executable", path.display()))?;
            }
        }
        Ok(())
    })();
    let _ = volume.remove().wait();
    built
}

/// Copy the directory `from` on `volume`, a path ending in `/`, to `to` here.
fn fetch(volume: &Volume, from: &str, to: &Path) -> anyhow::Result<()> {
    fs::create_dir_all(to).with_context(|| format!("creating {}", to.display()))?;
    for entry in volume.read_dir(from).wait()? {
        let path = format!("{from}{}", entry.name);
        if entry.metadata.is_dir {
            fetch(volume, &format!("{path}/"), &to.join(&entry.name))?;
        } else {
            let data = volume.read_file(&path, 0, None).wait()?;
            fs::write(to.join(&entry.name), data)
                .with_context(|| format!("writing {}", to.join(&entry.name).display()))?;
        }
    }
    Ok(())
}
