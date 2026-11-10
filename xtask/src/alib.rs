//! `cargo xtask build alib`: what the guest's `/alib` holds, built from source in virtx
//! machines into `build/alib/<arch>/`, where `install`, `archive` and a server about to boot
//! take it from -- as they take the guest kernel from `build/krunfw/`.
//!
//! Each directory under `virtx-alibs/` is one part, built by the `build.sh` in it:
//!
//! ```text
//! build.sh   sh build.sh OUT SOURCES, as root: writes into OUT what it puts in /alib
//! images     a line per build: where under /alib it goes, `.` for /alib itself, and the
//!            image it runs in, with {arch} for the guest's architecture
//! sources    a line per file it builds from: the name, the URL and the sha256
//! ```
//!
//! In a machine rather than on this host: what it builds is for the guest, whose architecture
//! the machine has whatever this host is, and against the C library of the image it runs in.
//! The sources are fetched here, into `build/alib/sources/`, checked, and handed in, so the
//! machine fetches nothing but its distribution's packages.

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, bail, ensure};
use sha2::{Digest as _, Sha256};
use virtx::{
    Client, Files as _, ImageJobEntry, SandboxCreateArgs, SandboxExecArgs, SandboxJobEntry,
};

/// Where the build puts `/alib` for this host's guest.
pub fn dir() -> anyhow::Result<PathBuf> {
    let arch = crate::guest_target()?.split('-').next().unwrap_or_default();
    Ok(crate::root_path().join("build/alib").join(arch))
}

/// Build every part on the client library at `library`, and the server at `server` if given,
/// else `virtx-stdio` of the library's version.
pub fn build(library: &Path, server: Option<&Path>) -> anyhow::Result<()> {
    virtx::load(library).with_context(|| format!("loading {}", library.display()))?;
    let client = match server {
        Some(server) => Client::spawn(server.to_string_lossy(), [] as [&str; 0]),
        None => Client::spawn_virtx([] as [&str; 0]),
    }
    .context("starting a virtx server")?;

    let arch = crate::guest_target()?.split('-').next().unwrap_or_default();
    // Emptied first, as every part is built again into it, some at its top.
    let out = dir()?;
    if out.exists() {
        fs::remove_dir_all(&out).with_context(|| format!("removing {}", out.display()))?;
    }
    let parts = crate::root_path().join("virtx-alibs");
    let mut dirs = fs::read_dir(&parts)
        .with_context(|| format!("reading {}", parts.display()))?
        .map(|entry| entry.map(|e| e.path()))
        .collect::<Result<Vec<_>, _>>()?;
    dirs.sort();
    for dir in dirs.iter().filter(|dir| dir.is_dir()) {
        let images = dir.join("images");
        let text =
            fs::read_to_string(&images).with_context(|| format!("reading {}", images.display()))?;
        for line in text
            .lines()
            .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        {
            let [part, image] = line.split_whitespace().collect::<Vec<_>>()[..] else {
                bail!("{}: not `directory image`: {line}", images.display());
            };
            build_part(
                &client,
                dir,
                &image.replace("{arch}", arch),
                &out.join(part),
            )?;
        }
    }
    Ok(())
}

/// The files `dir/sources` lists, fetched into `build/alib/sources/` the first time and checked
/// against their sums every time. None where there is no `sources`.
fn fetch_sources(dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let list = dir.join("sources");
    if !list.is_file() {
        return Ok(Vec::new());
    }
    let cache = crate::root_path().join("build/alib/sources");
    fs::create_dir_all(&cache).with_context(|| format!("creating {}", cache.display()))?;
    let mut files = Vec::new();
    let text = fs::read_to_string(&list).with_context(|| format!("reading {}", list.display()))?;
    for line in text
        .lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
    {
        let [name, url, sum] = line.split_whitespace().collect::<Vec<_>>()[..] else {
            bail!("{}: not `file url sha256`: {line}", list.display());
        };
        let file = cache.join(name);
        if !file.is_file() {
            eprintln!("xtask: fetching {url}");
            let part = file.with_extension("part");
            let status = Command::new("curl")
                .args(["-fsSL", "--retry", "3", "-o"])
                .arg(&part)
                .arg(url)
                .status()
                .context("running curl")?;
            ensure!(status.success(), "fetching {url} failed");
            fs::rename(&part, &file).with_context(|| format!("placing {}", file.display()))?;
        }
        let data = fs::read(&file).with_context(|| format!("reading {}", file.display()))?;
        let actual = format!("{:x}", Sha256::digest(&data));
        if actual != sum {
            let _ = fs::remove_file(&file);
            bail!("{name} is {actual}, not the {sum} {} says", list.display());
        }
        files.push(file);
    }
    Ok(files)
}

/// Run `dir/build.sh OUT SOURCES` in a machine of `image`, with `dir`'s files at `/build` and
/// its sources at `/build/tarballs`, and unpack what it wrote into `out`. Through a tar on the
/// volume, which keeps the files' modes that a volume's own files do not.
fn build_part(client: &Client, dir: &Path, image: &str, out: &Path) -> anyhow::Result<()> {
    let sources = fetch_sources(dir)?;
    eprintln!("xtask: pulling {image}");
    let mut pull = client.pull_image(image, None).log();
    while let Some(entry) = pull.wait_next() {
        if let ImageJobEntry::End(end) = entry?.entry
            && let Some(error) = end.error
        {
            bail!("pulling {image}: {error}");
        }
    }

    let volume = client.create_volume(None::<String>).wait()?;
    let tar = out.join(".build.tar");
    let built = (|| {
        for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
            let path = entry?.path();
            if path.is_file() {
                let name = path
                    .file_name()
                    .expect("a file has a name")
                    .to_string_lossy()
                    .into_owned();
                let data =
                    fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
                volume.write_file(&name, data, None).wait()?;
            }
        }
        volume.create_dir("tarballs").wait()?;
        for path in &sources {
            let name = path
                .file_name()
                .expect("a source is a file")
                .to_string_lossy();
            let data = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
            volume
                .write_file(format!("tarballs/{name}"), data, None)
                .wait()?;
        }
        let vcpus = std::thread::available_parallelism().map_or(2, |n| n.get().min(8)) as u8;
        let sandbox = client
            .create_sandbox(
                &SandboxCreateArgs::new(image)
                    .vcpus(vcpus)
                    .memory_mib(4096)
                    .volume(volume.name(), "/build"),
            )
            .wait()?;
        // On the machine's own disk, and onto the volume once it is whole.
        let mut log = sandbox
            .exec(&SandboxExecArgs::new([
                "sh",
                "-c",
                "sh /build/build.sh /tmp/out /build/tarballs && tar -C /tmp/out -cf /build/out.tar .",
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
        ensure!(
            code == Some(0),
            "{}/build.sh failed (exit {code:?})",
            dir.display()
        );
        let data = volume.read_file("out.tar", 0, None).wait()?;
        fs::create_dir_all(out).with_context(|| format!("creating {}", out.display()))?;
        fs::write(&tar, data).with_context(|| format!("writing {}", tar.display()))
    })();
    let _ = volume.remove().wait();
    built?;

    // A relative path, run from where it unpacks: Git Bash's GNU tar reads `C:` as a remote host.
    let status = Command::new("tar")
        .current_dir(out)
        .arg("-xf")
        .arg(tar.file_name().expect("the tar is a file"))
        .status()
        .context("running tar")?;
    ensure!(status.success(), "unpacking {} failed", tar.display());
    fs::remove_file(&tar).with_context(|| format!("removing {}", tar.display()))
}
