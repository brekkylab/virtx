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
//!
//! What each build wrote is kept as a tar in `build/alib/parts/<arch>/`, beside the sum of what
//! it was built from -- the part's files and the image -- and built again only when that sum
//! changes. `build/alib/<arch>/` is unpacked from them every time.

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

/// Each build's tar and the sum it was built from, as `<part>-<line>.tar` and `.sum`.
const PARTS: &str = "build/alib/parts";

/// Where the build puts `/alib` for this host's guest.
pub fn dir() -> anyhow::Result<PathBuf> {
    let arch = crate::guest_target()?.split('-').next().unwrap_or_default();
    Ok(crate::root_path().join("build/alib").join(arch))
}

/// Build every part whose files or image changed since it was last built, on the client library
/// at `library` and the server at `server`, then unpack them all into [`dir`].
pub fn build(library: &Path, server: &Path) -> anyhow::Result<()> {
    let arch = crate::guest_target()?.split('-').next().unwrap_or_default();
    let built = crate::root_path().join(PARTS).join(arch);
    fs::create_dir_all(&built).with_context(|| format!("creating {}", built.display()))?;
    // Started by the first part that is built, and by none when every one is as it was.
    let mut client = None;
    // Emptied first, as some parts unpack at its top.
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
        let lines = text
            .lines()
            .filter(|l| !l.starts_with('#') && !l.trim().is_empty());
        for (n, line) in lines.enumerate() {
            let [part, image] = line.split_whitespace().collect::<Vec<_>>()[..] else {
                bail!("{}: not `directory image`: {line}", images.display());
            };
            let image = image.replace("{arch}", arch);
            let name = dir
                .file_name()
                .expect("a part is a directory")
                .to_string_lossy();
            let tar = built.join(format!("{name}-{n}.tar"));
            let sum_file = tar.with_extension("sum");
            let sum = sum(dir, &image)?;
            if tar.is_file() && fs::read_to_string(&sum_file).ok().as_deref() == Some(&sum) {
                eprintln!("xtask: {name} for {image} is as it was built");
            } else {
                if client.is_none() {
                    virtx::load(library)
                        .with_context(|| format!("loading {}", library.display()))?;
                    let spawned = Client::spawn(server.to_string_lossy(), [] as [&str; 0])
                        .context("starting a virtx server")?;
                    client = Some(spawned);
                }
                let _ = fs::remove_file(&sum_file);
                build_part(client.as_ref().expect("started above"), dir, &image, &tar)?;
                fs::write(&sum_file, &sum)
                    .with_context(|| format!("writing {}", sum_file.display()))?;
            }
            // On its stdin, run from where it unpacks: Git Bash's GNU tar reads `C:` in a path
            // as a remote host.
            let at = out.join(part);
            fs::create_dir_all(&at).with_context(|| format!("creating {}", at.display()))?;
            let file =
                fs::File::open(&tar).with_context(|| format!("opening {}", tar.display()))?;
            let status = Command::new("tar")
                .current_dir(&at)
                .arg("-xf")
                .arg("-")
                .stdin(file)
                .status()
                .context("running tar")?;
            ensure!(status.success(), "unpacking {} failed", tar.display());
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

/// What `dir`'s part is built from in `image`: the sha256 of the image's name and of each of the
/// part's files, by name.
fn sum(dir: &Path, image: &str) -> anyhow::Result<String> {
    let mut files = fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .map(|entry| entry.map(|e| e.path()))
        .collect::<Result<Vec<_>, _>>()?;
    files.sort();
    let mut hash = Sha256::new();
    hash.update(image);
    for path in files.iter().filter(|path| path.is_file()) {
        let data = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        hash.update([0]);
        hash.update(
            path.file_name()
                .expect("a file has a name")
                .as_encoded_bytes(),
        );
        hash.update([0]);
        hash.update(Sha256::digest(&data));
    }
    Ok(format!("{:x}", hash.finalize()))
}

/// Run `dir/build.sh OUT SOURCES` in a machine of `image`, with `dir`'s files at `/build` and
/// its sources at `/build/tarballs`, and write what it wrote to `tar`: a tar, which keeps the
/// files' modes that a volume's own files do not.
fn build_part(client: &Client, dir: &Path, image: &str, tar: &Path) -> anyhow::Result<()> {
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
        fs::write(tar, data).with_context(|| format!("writing {}", tar.display()))
    })();
    let _ = volume.remove().wait();
    built
}
