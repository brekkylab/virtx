//! Reaching the server: `virtx-uvm` started here, over its stdin and stdout.

use std::{io, path::PathBuf};

use clap::Args;
use tokio::process::Command;
use virtx_core::Client;

/// The server's program, as virtx-uvm installs it.
const PROGRAM: &str = "virtx-uvm";

#[derive(Debug, PartialEq, Args)]
pub(crate) struct ServerArgs {
    /// The server to start; `virtx-uvm` in `$VIRTX_HOME/bin`, else on `PATH`, if absent.
    #[arg(long, env = "VIRTX_SERVER", global = true, value_name = "PROGRAM")]
    pub(crate) server: Option<PathBuf>,
}

/// A client of the server `args` names.
pub(crate) fn connect(args: &ServerArgs) -> anyhow::Result<Client> {
    let program = args.server.clone().unwrap_or_else(default_program);
    Client::spawn(Command::new(&program)).map_err(|error| {
        let started = format!("starting {}", program.display());
        if error.kind() == io::ErrorKind::NotFound {
            anyhow::Error::new(error).context(format!(
                "{started}: install it with `cargo xtask install` in virtx-uvm, or name it \
                 with --server"
            ))
        } else {
            anyhow::Error::new(error).context(started)
        }
    })
}

/// `virtx-uvm` where virtx-uvm's `cargo xtask install` puts it, if it is there, else as `PATH`
/// finds it.
fn default_program() -> PathBuf {
    let name = format!("{PROGRAM}{}", std::env::consts::EXE_SUFFIX);
    cache_root()
        .map(|root| root.join("bin").join(&name))
        .filter(|path| path.is_file())
        .unwrap_or_else(|| PathBuf::from(PROGRAM))
}

/// Where virtx-uvm keeps what it keeps on this host: `$VIRTX_HOME`, or a `virtx` directory in
/// the platform's cache directory. The same as `virtx_uvm::cache_root`.
fn cache_root() -> Option<PathBuf> {
    let var = |name| std::env::var_os(name).map(PathBuf::from);
    if let Some(home) = var("VIRTX_HOME") {
        return Some(home);
    }
    let cache = if cfg!(windows) {
        var("LOCALAPPDATA")
            .or_else(|| var("USERPROFILE").map(|home| home.join("AppData").join("Local")))?
    } else if cfg!(target_os = "macos") {
        var("HOME")?.join("Library").join("Caches")
    } else {
        var("XDG_CACHE_HOME").or_else(|| var("HOME").map(|home| home.join(".cache")))?
    };
    Some(cache.join("virtx"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_server_says_how_to_get_one() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _guard = runtime.enter();
        let args = ServerArgs {
            server: Some("/nonexistent/virtx-uvm".into()),
        };
        let error = connect(&args).err().unwrap();
        assert!(
            format!("{error:#}").contains("cargo xtask install"),
            "{error:#}"
        );
    }
}
