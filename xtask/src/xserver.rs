//! `cargo xtask build xserver`: the X server `display` runs, built from source by
//! `virtx-abins/display/xserver/build.sh` in an Alpine machine, into
//! `build/display-xserver/<arch>/`, where `build abin` takes it from.
//!
//! In a virtx machine rather than on this host: the X server is static musl executables, which
//! Alpine has the archives for, and the machine is the guest's architecture whatever this
//! host is. The X.org tarballs it builds are downloaded here, into
//! `build/display-xserver/sources/` once, and handed in: the machine needs the network for
//! Alpine's packages only.

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, bail, ensure};
use virtx::{
    Client, Files as _, ImageJobEntry, SandboxCreateArgs, SandboxExecArgs, SandboxJobEntry, Volume,
};

/// The image `build.sh` runs in.
const BASE: &str = "alpine:3.22";
/// What the build writes that runs; a volume's files come back without their modes.
const EXECUTABLES: &[&str] = &["Xvfb", "xkbcomp"];

/// Build it with the client library at `library`, on the server at `server` if given and on
/// `virtx-stdio` of the library's version otherwise, into `out`.
pub fn build(library: &Path, server: Option<&Path>, out: &Path) -> anyhow::Result<()> {
    let dir = crate::root_path().join("virtx-abins/display/xserver");
    let tarballs = download(&dir.join("sources"))?;
    virtx::load(library).with_context(|| format!("loading {}", library.display()))?;
    let client = match server {
        Some(server) => Client::spawn(server.to_string_lossy(), [] as [&str; 0]),
        None => Client::spawn_virtx([] as [&str; 0]),
    }
    .context("starting a virtx server")?;

    eprintln!("xtask: pulling {BASE}");
    let mut pull = client.pull_image(BASE, None).log();
    while let Some(entry) = pull.wait_next() {
        if let ImageJobEntry::End(end) = entry?.entry
            && let Some(error) = end.error
        {
            bail!("pulling {BASE}: {error}");
        }
    }

    let volume = client.create_volume(None::<String>).wait()?;
    let built = upload(&volume, &dir, &tarballs).and_then(|()| build_on(&client, &volume, out));
    let _ = volume.remove().wait();
    built
}

/// The tarballs `sources` lists, downloaded into the cache beside the build's output the first
/// time. `build.sh` checks each against its checksum.
fn download(sources: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let list =
        fs::read_to_string(sources).with_context(|| format!("reading {}", sources.display()))?;
    let cache = crate::root_path().join("build/display-xserver/sources");
    fs::create_dir_all(&cache).with_context(|| format!("creating {}", cache.display()))?;
    let mut tarballs = Vec::new();
    for line in list
        .lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
    {
        let mut fields = line.split_whitespace();
        let (Some(name), Some(dir)) = (fields.next(), fields.next()) else {
            bail!(
                "{}: a line without a name and a directory: {line}",
                sources.display()
            );
        };
        let tarball = cache.join(format!("{name}.tar.xz"));
        if !tarball.is_file() {
            let url = format!("https://www.x.org/releases/individual/{dir}/{name}.tar.xz");
            eprintln!("xtask: downloading {url}");
            let part = tarball.with_extension("part");
            let mut curl = Command::new("curl");
            curl.args(["-fsSL", "--retry", "3", "-o"])
                .arg(&part)
                .arg(&url);
            let status = curl.status().context("running curl")?;
            ensure!(status.success(), "downloading {url} failed ({status})");
            fs::rename(&part, &tarball)?;
        }
        tarballs.push(tarball);
    }
    Ok(tarballs)
}

/// The script, its list of sources and the tarballs onto `volume`.
fn upload(volume: &Volume, dir: &Path, tarballs: &[PathBuf]) -> anyhow::Result<()> {
    for name in ["build.sh", "sources"] {
        let data = fs::read(dir.join(name))
            .with_context(|| format!("reading {}", dir.join(name).display()))?;
        volume.write_file(name, data, None).wait()?;
    }
    volume.create_dir("tarballs").wait()?;
    for tarball in tarballs {
        let name = tarball.file_name().unwrap().to_string_lossy();
        let data = fs::read(tarball).with_context(|| format!("reading {}", tarball.display()))?;
        volume
            .write_file(format!("tarballs/{name}"), data, None)
            .wait()?;
    }
    Ok(())
}

fn build_on(client: &Client, volume: &Volume, out: &Path) -> anyhow::Result<()> {
    let vcpus = std::thread::available_parallelism().map_or(2, |n| n.get().min(8)) as u8;
    let sandbox = client
        .create_sandbox(
            &SandboxCreateArgs::new(BASE)
                .vcpus(vcpus)
                .memory_mib(4096)
                .volume(volume.name(), "/build"),
        )
        .wait()?;

    let built = (|| {
        eprintln!("xtask: building the X server in {BASE}");
        let mut log = sandbox
            .exec(&SandboxExecArgs::new([
                "sh",
                "-c",
                "sh /build/build.sh /tmp/out /build/tarballs && cp -R /tmp/out /build/out",
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
        ensure!(code == Some(0), "build.sh failed (exit {code:?})");

        if out.exists() {
            fs::remove_dir_all(out).with_context(|| format!("removing {}", out.display()))?;
        }
        fetch(volume, "out/", out)?;
        for name in EXECUTABLES {
            executable(&out.join(name))?;
        }
        Ok(())
    })();
    let _ = sandbox.remove().wait();
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

fn executable(path: &Path) -> anyhow::Result<()> {
    ensure!(path.is_file(), "the build wrote no {}", path.display());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755))
            .with_context(|| format!("making {} executable", path.display()))?;
    }
    Ok(())
}
