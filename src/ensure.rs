//! Fetching the console server when this host has none.

use std::path::{Path, PathBuf};

use anyhow::Context as _;

use crate::cache_root;

/// The server release this build fetches by default, set at build time -- a sha or a tag.
/// `None` in a build nobody pinned, which follows `latest`.
const PINNED: Option<&str> = match option_env!("VIRTX_UVM_PINNED_VERSION") {
    Some(version) if !version.is_empty() => Some(version),
    _ => None,
};

/// Where releases are fetched from unless `$VIRTX_DIST_URL` says otherwise.
const DIST_URL: &str = "https://virtx-dist.s3.us-east-1.amazonaws.com";

/// The file in `bin/` naming the release it holds.
const RELEASE: &str = ".release";

/// What [`RELEASE`] reads for a server built here (`cargo xtask install`), never replaced.
const LOCAL: &str = "local";

/// Fetch the console server into [`cache_root`]`/bin` unless it holds the release wanted, and
/// return that directory.
///
/// **The release wanted, or one built here**: `bin/.release` names what `bin/` holds, and a
/// server whose name differs from the release wanted, or that has none, is fetched again. One
/// `cargo xtask install` put there reads `local` and is left alone. `latest` is not looked up:
/// a server fetched as `latest` stays until another release is asked for.
///
/// The release is `$VIRTX_UVM_VERSION` if set; else the one this build was pinned to
/// (`VIRTX_UVM_PINNED_VERSION`, set for a published package so it fetches the server it was
/// tested with); else `latest`. `virtx-uvm` is placed last, so a partial fetch leaves a
/// `bin/` the next call refetches rather than one that looks complete.
///
/// # Release layout
///
/// One archive per platform, holding what `bin/` needs, under the bucket's public HTTPS
/// endpoint:
///
/// ```text
/// virtx-uvm/<ref>/virtx-uvm-<os>-<arch>.tar.gz
/// ```
///
/// `<os>`/`<arch>` are as [`std::env::consts`] spells them. `<ref>` is any name a release goes
/// by (a virtx-uvm git sha, a version tag, or `latest`), so fetching is one URL whichever
/// name is given.
pub async fn ensure_virtx() -> anyhow::Result<PathBuf> {
    let root = cache_root();
    let bin = root.join("bin");
    let server = format!("virtx-uvm{}", std::env::consts::EXE_SUFFIX);

    let platform = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);
    let base = std::env::var("VIRTX_DIST_URL")
        .ok()
        .filter(|url| !url.is_empty())
        .unwrap_or_else(|| DIST_URL.to_string());
    let release = std::env::var("VIRTX_UVM_VERSION")
        .ok()
        .filter(|v| !v.is_empty())
        .or_else(|| PINNED.map(str::to_string))
        .unwrap_or_else(|| "latest".to_string());
    let held = std::fs::read_to_string(bin.join(RELEASE)).ok();
    if bin.join(&server).is_file() && current(held.as_deref(), &release) {
        return Ok(bin);
    }

    let url = format!(
        "{}/virtx-uvm/{release}/virtx-uvm-{platform}.tar.gz",
        base.trim_end_matches('/')
    );
    let archive = fetch(&url).await.with_context(|| {
        format!("no virtx-uvm release is published for {platform} as `{release}`")
    })?;
    tokio::task::spawn_blocking(move || unpack(&archive, &root, &server))
        .await
        .context("unpacking virtx-uvm panicked")??;
    // After the server, so a fetch cut short is fetched again.
    std::fs::write(bin.join(RELEASE), &release)
        .with_context(|| format!("writing {}", bin.join(RELEASE).display()))?;
    Ok(bin)
}

/// Whether a `bin/` whose [`RELEASE`] reads `held` is the one to keep when `wanted` is asked for.
fn current(held: Option<&str>, wanted: &str) -> bool {
    matches!(held.map(str::trim), Some(held) if held == LOCAL || held == wanted)
}

/// GET `url`, whole. A bucket without public listing answers a missing key with 403, not 404,
/// so a failure reports only the URL and status, without interpreting it.
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

/// Unpack `archive` beside `bin/` and move its entries in, `server` last.
///
/// The staging directory is per-process and on the same filesystem, so every move is a rename
/// and no reader of `bin/` sees a half-written file.
fn unpack(archive: &[u8], root: &Path, server: &str) -> anyhow::Result<()> {
    let part = root.join(format!(".bin.{}.part", std::process::id()));
    let _ = std::fs::remove_dir_all(&part);
    std::fs::create_dir_all(&part).with_context(|| format!("creating {}", part.display()))?;
    let moved = (|| {
        tar::Archive::new(flate2::read::GzDecoder::new(archive))
            .unpack(&part)
            .context("unpacking the virtx-uvm archive")?;
        anyhow::ensure!(
            part.join(server).is_file(),
            "the virtx-uvm archive has no {server}"
        );

        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).with_context(|| format!("creating {}", bin.display()))?;
        for entry in std::fs::read_dir(&part)? {
            let entry = entry?;
            if entry.file_name() != server {
                replace(&entry.path(), &bin.join(entry.file_name()))?;
            }
        }
        replace(&part.join(server), &bin.join(server))
    })();
    let _ = std::fs::remove_dir_all(&part);
    moved
}

/// Rename `from` over `to`. A rename replaces a file but not a directory, so an old directory
/// is removed first.
fn replace(from: &Path, to: &Path) -> anyhow::Result<()> {
    if from.is_dir() && to.is_dir() {
        std::fs::remove_dir_all(to).with_context(|| format!("removing {}", to.display()))?;
    }
    std::fs::rename(from, to)
        .with_context(|| format!("moving {} to {}", from.display(), to.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_server_is_kept_only_as_the_release_wanted_or_built_here() {
        assert!(current(Some("v0.1.2\n"), "v0.1.2"));
        assert!(current(Some("local"), "v0.1.2"));
        assert!(current(Some("latest"), "latest"));
        assert!(!current(Some("v0.1.1"), "v0.1.2"));
        assert!(!current(Some("latest"), "v0.1.2"));
        // Fetched before servers were named, or installed by hand.
        assert!(!current(None, "latest"));
    }
}
