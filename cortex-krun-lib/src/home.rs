//! Where cortex-krun keeps things on the machine it is running on.

use std::path::PathBuf;

/// Everything cortex-krun keeps on this host, under one root: `$CORTEX_KRUN_HOME`, or
/// `cortex` under the user's cache directory.
///
/// **One rule for every kind of thing kept here**, because a host has one answer to "where
/// does this go" and two ways of deriving it are two answers. What hangs off it is split by
/// what owns it -- images below, which every tool in this repository shares, and one
/// directory per live server process, which no other process has any business in -- and that
/// split is a path under the root rather than a root of its own.
///
/// The user's cache directory is `XDG_CACHE_HOME` or `~/.cache`, and on Windows
/// `%LOCALAPPDATA%`; a home directory is where it is looked for when the variable naming it
/// is unset. `None` when even that is not to be had, which a caller answers for itself:
/// there is no default that is right for both a cache that can be rebuilt by downloading it
/// again and a session's own disk.
pub fn home() -> Option<PathBuf> {
    if let Some(named) = std::env::var_os("CORTEX_KRUN_HOME") {
        return Some(PathBuf::from(named));
    }

    #[cfg(windows)]
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("USERPROFILE")
                .map(|home| PathBuf::from(home).join("AppData").join("Local"))
        });

    #[cfg(not(windows))]
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")));

    Some(base?.join("cortex"))
}
