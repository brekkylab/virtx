//! Fetching the server this client matches, when this host has none.

use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::Context as _;
use sha2::{Digest, Sha256};

/// Where a release's assets hang, unless `$VIRTX_DIST_URL` says otherwise:
/// `<base>/v<version>/<asset>`.
const DIST_URL: &str = "https://github.com/brekkylab/virtx/releases/download";

/// The server's file name on this host.
fn server_file() -> String {
    format!("virtx-stdio{}", std::env::consts::EXE_SUFFIX)
}

/// The path of `virtx-stdio`, the server of this client's version, fetched into the cache if it
/// is not there yet.
///
/// The server and the client are released together, under one tag, so the server a client
/// matches is the one of its own version: `v<version>` of this crate, and `$VIRTX_STDIO_VERSION`
/// to name another. It is kept under [`cache_root`]`/virtx-stdio/<version>/bin/`, one directory
/// per version, so a client that is upgraded fetches its own server and never runs an older
/// one left behind. **Present is enough**: a version's `bin/` that has `virtx-stdio` is left
/// alone.
///
/// `$VIRTX_STDIO` names a server to use as it is, fetching nothing -- one built here, say.
///
/// # Release layout
///
/// One archive per platform among a release's assets, holding what `bin/` needs, and
/// `SHA256SUMS` over the archives, which the archive is checked against before anything is
/// placed:
///
/// ```text
/// <base>/v<version>/virtx-stdio-<os>-<arch>.tar.gz
/// <base>/v<version>/SHA256SUMS
/// ```
///
/// `<os>`/`<arch>` are as [`std::env::consts`] spells them. `<base>` is GitHub's releases
/// unless `$VIRTX_DIST_URL` says otherwise.
pub async fn ensure_virtx() -> anyhow::Result<PathBuf> {
    let env = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
    if let Some(named) = env("VIRTX_STDIO") {
        let path = PathBuf::from(named);
        anyhow::ensure!(
            path.is_file(),
            "$VIRTX_STDIO: {} is not a file",
            path.display()
        );
        return Ok(path);
    }
    let version = env("VIRTX_STDIO_VERSION").unwrap_or_else(|| env!("CARGO_PKG_VERSION").into());
    let base = env("VIRTX_DIST_URL").unwrap_or_else(|| DIST_URL.into());
    let root = cache_root().join("virtx-stdio").join(&version);
    ensure_in(&root, &base, &version).await
}

/// [`ensure_virtx`] for the version `version`, kept under `root`, from the release at
/// `<base>/v<version>/`.
///
/// Fetching is one process at a time, under a lock file in `root`, so concurrent callers --
/// threads of one process or processes -- fetch once between them and the rest find the
/// server placed. A fetch unpacks into a staging directory of its own in `root` and moves
/// each entry into `bin/` by rename, the server last: a reader of `bin/` sees whole files
/// only, and one that finds the server finds the rest.
async fn ensure_in(root: &Path, base: &str, version: &str) -> anyhow::Result<PathBuf> {
    let server = server_file();
    let bin = root.join("bin");
    let placed = bin.join(&server);
    if placed.is_file() {
        return Ok(placed);
    }

    fs::create_dir_all(root).with_context(|| format!("creating {}", root.display()))?;
    let lock_path = root.join(".lock");
    let _lock = tokio::task::spawn_blocking(move || -> anyhow::Result<fs::File> {
        let lock = fs::File::create(&lock_path)
            .with_context(|| format!("creating {}", lock_path.display()))?;
        lock.lock()
            .with_context(|| format!("locking {}", lock_path.display()))?;
        Ok(lock)
    })
    .await
    .context("locking panicked")??;
    // Another process may have placed it while this one waited.
    if placed.is_file() {
        return Ok(placed);
    }

    let platform = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);
    let asset = format!("virtx-stdio-{platform}.tar.gz");
    let release = format!("{}/v{version}", base.trim_end_matches('/'));
    let sums = fetch(&format!("{release}/SHA256SUMS"))
        .await
        .with_context(|| format!("no virtx-stdio release v{version} is published"))?;
    let expected = checksum_for(&sums, &asset).with_context(|| {
        format!("no virtx-stdio release v{version} is published for {platform}")
    })?;
    let archive = fetch(&format!("{release}/{asset}"))
        .await
        .with_context(|| {
            format!("no virtx-stdio release v{version} is published for {platform}")
        })?;
    let actual = format!("{:x}", Sha256::digest(&archive));
    anyhow::ensure!(
        actual == expected,
        "{asset} of v{version} does not match its SHA256SUMS: {actual} != {expected}"
    );

    let root = root.to_path_buf();
    tokio::task::spawn_blocking(move || unpack(&archive, &root, &server))
        .await
        .context("unpacking virtx-stdio panicked")??;
    Ok(placed)
}

/// GET `url`, whole.
async fn fetch(url: &str) -> anyhow::Result<Vec<u8>> {
    let response = reqwest::get(url)
        .await
        .with_context(|| format!("fetching {url}"))?;
    let status = response.status();
    anyhow::ensure!(status.is_success(), "fetching {url}: {status}");
    let body = response
        .bytes()
        .await
        .with_context(|| format!("reading {url}"))?;
    Ok(body.to_vec())
}

/// The checksum `sums`, a `SHA256SUMS` as `sha256sum` writes it, gives for `name`.
fn checksum_for(sums: &[u8], name: &str) -> anyhow::Result<String> {
    let sums = std::str::from_utf8(sums).context("SHA256SUMS is not UTF-8")?;
    sums.lines()
        .filter_map(|line| {
            let (sum, file) = line.split_once(' ')?;
            (file.trim_start_matches([' ', '*']) == name).then(|| sum.to_string())
        })
        .next()
        .with_context(|| format!("SHA256SUMS has no {name}"))
}

/// Unpack `archive` into a staging directory in `root` and move its entries into `bin/`, `server`
/// last. The staging directory is this process's own and on the same filesystem, so every move
/// is a rename.
fn unpack(archive: &[u8], root: &Path, server: &str) -> anyhow::Result<()> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let part = root.join(format!(".part-{}-{nanos}", std::process::id()));
    fs::create_dir_all(&part).with_context(|| format!("creating {}", part.display()))?;
    let moved = (|| {
        tar::Archive::new(flate2::read::GzDecoder::new(archive))
            .unpack(&part)
            .context("unpacking the virtx-stdio archive")?;
        anyhow::ensure!(
            part.join(server).is_file(),
            "the virtx-stdio archive has no {server}"
        );

        let bin = root.join("bin");
        fs::create_dir_all(&bin).with_context(|| format!("creating {}", bin.display()))?;
        for entry in fs::read_dir(&part).with_context(|| format!("reading {}", part.display()))? {
            let entry = entry?;
            if entry.file_name() != server {
                replace(&entry.path(), &bin.join(entry.file_name()))?;
            }
        }
        replace(&part.join(server), &bin.join(server))
    })();
    let _ = fs::remove_dir_all(&part);
    moved
}

/// Rename `from` over `to`. A rename replaces a file but not a directory, so an old directory
/// is removed first.
fn replace(from: &Path, to: &Path) -> anyhow::Result<()> {
    if from.is_dir() && to.is_dir() {
        fs::remove_dir_all(to).with_context(|| format!("removing {}", to.display()))?;
    }
    fs::rename(from, to).with_context(|| format!("moving {} to {}", from.display(), to.display()))
}

/// Where virtx keeps what it keeps on this host: `$VIRTX_HOME`, or a `virtx` directory in the
/// platform's cache directory -- `~/Library/Caches` on macOS, `%LOCALAPPDATA%` (else
/// `%USERPROFILE%\AppData\Local`) on Windows, `$XDG_CACHE_HOME` or `~/.cache` elsewhere.
pub fn cache_root() -> PathBuf {
    if let Some(named) = std::env::var_os("VIRTX_HOME") {
        return PathBuf::from(named);
    }
    #[cfg(windows)]
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("USERPROFILE").unwrap_or_default())
                .join("AppData")
                .join("Local")
        });
    #[cfg(target_os = "macos")]
    let base = PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
        .join("Library")
        .join("Caches");
    #[cfg(not(any(windows, target_os = "macos")))]
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".cache")
        });
    base.join("virtx")
}

#[cfg(test)]
mod tests {
    use std::{
        net::SocketAddr,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering::SeqCst},
        },
    };

    use axum::{Router, extract::State, routing::get};

    use super::*;

    /// A release at `v1.2.3` of an archive holding `virtx-stdio`, `libkrunfw` and `abin/`, served
    /// over HTTP with its `SHA256SUMS`, counting the archive's fetches.
    struct Release {
        base: String,
        fetches: Arc<AtomicUsize>,
    }

    async fn release(sums: Option<Vec<u8>>) -> Release {
        let server = server_file();
        let mut tar = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::fast(),
        ));
        let mut file = |name: &str, data: &[u8]| {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            tar.append_data(&mut header, name, data).unwrap();
        };
        file(&server, b"#!/bin/sh\necho server\n");
        file("libkrunfw.5.dylib", b"kernel");
        file("abin/mem", b"mem");
        let archive = tar.into_inner().unwrap().finish().unwrap();
        let asset = format!(
            "virtx-stdio-{}-{}.tar.gz",
            std::env::consts::OS,
            std::env::consts::ARCH
        );
        let sums = sums
            .unwrap_or_else(|| format!("{:x}  {asset}\n", Sha256::digest(&archive)).into_bytes());

        let fetches = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&fetches);
        let app = Router::new()
            .route("/v1.2.3/SHA256SUMS", get(async move || sums.clone()))
            .route(
                &format!("/v1.2.3/{asset}"),
                get(async move |State(fetches): State<Arc<AtomicUsize>>| {
                    fetches.fetch_add(1, SeqCst);
                    archive.clone()
                }),
            )
            .with_state(counted);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address: SocketAddr = listener.local_addr().unwrap();
        tokio::spawn(axum::serve(listener, app).into_future());
        Release {
            base: format!("http://{address}"),
            fetches,
        }
    }

    fn fresh_root() -> PathBuf {
        std::env::temp_dir().join(format!("virtx-ensure-{}", uuid::Uuid::new_v4()))
    }

    #[tokio::test]
    async fn fetches_once_and_places_the_whole_set() {
        let release = release(None).await;
        let root = fresh_root();

        let placed = ensure_in(&root, &release.base, "1.2.3").await.unwrap();
        assert_eq!(placed, root.join("bin").join(server_file()));
        assert_eq!(fs::read(&placed).unwrap(), b"#!/bin/sh\necho server\n");
        assert_eq!(
            fs::read(root.join("bin/libkrunfw.5.dylib")).unwrap(),
            b"kernel"
        );
        assert_eq!(fs::read(root.join("bin/abin/mem")).unwrap(), b"mem");
        assert!(
            !fs::read_dir(&root).unwrap().any(|e| e
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".part")),
            "the staging directory is gone"
        );

        let again = ensure_in(&root, &release.base, "1.2.3").await.unwrap();
        assert_eq!(again, placed);
        assert_eq!(release.fetches.load(SeqCst), 1, "present is enough");
        fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test]
    async fn concurrent_calls_fetch_once_between_them() {
        let release = release(None).await;
        let root = fresh_root();

        let calls = (0..8).map(|_| {
            let (root, base) = (root.clone(), release.base.clone());
            tokio::spawn(async move { ensure_in(&root, &base, "1.2.3").await })
        });
        let mut placed = Vec::new();
        for call in calls {
            placed.push(call.await.unwrap().unwrap());
        }
        assert!(placed.iter().all(|p| *p == placed[0]));
        assert!(placed[0].is_file());
        assert_eq!(release.fetches.load(SeqCst), 1);
        fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test]
    async fn an_archive_that_does_not_match_its_sum_places_nothing() {
        let wrong = format!(
            "{:064x}  virtx-stdio-{}-{}.tar.gz\n",
            0,
            std::env::consts::OS,
            std::env::consts::ARCH
        );
        let release = release(Some(wrong.into_bytes())).await;
        let root = fresh_root();

        let error = ensure_in(&root, &release.base, "1.2.3")
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("does not match its SHA256SUMS"), "{error}");
        assert!(!root.join("bin").exists());
        fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test]
    async fn a_version_with_no_release_says_so() {
        let release = release(None).await;
        let root = fresh_root();

        let error = ensure_in(&root, &release.base, "9.9.9")
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("no virtx-stdio release v9.9.9"), "{error}");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_sum_is_found_by_its_file_name() {
        let sums = b"aa  virtx-stdio-a.tar.gz\nbb *virtx-stdio-b.tar.gz\n";
        assert_eq!(checksum_for(sums, "virtx-stdio-a.tar.gz").unwrap(), "aa");
        assert_eq!(checksum_for(sums, "virtx-stdio-b.tar.gz").unwrap(), "bb");
        assert!(checksum_for(sums, "virtx-stdio-c.tar.gz").is_err());
    }
}
