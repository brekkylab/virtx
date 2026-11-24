//! `browser`: vercel-labs/agent-browser, driving the Chromium installed with it.
//!
//! Started by a caller, this runs agent-browser with its arguments, naming this program as the
//! browser to launch. Started by agent-browser as that browser, it runs Chromium with the
//! libraries and fonts installed for it, which nothing else agent-browser starts is given.
//! `build.rs` says what is installed and why.

use std::{
    env,
    ffi::OsString,
    fs,
    os::unix::process::CommandExt as _,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, SystemTime},
};

/// Set for agent-browser, and so for the browser it launches: that this program is started as
/// the browser.
const AS_BROWSER: &str = "VIRTX_BROWSER_AS_BROWSER";

fn main() {
    let me = env::current_exe().expect("/proc/self/exe names this program");
    // What `build.rs` installs, in `.browser/` beside this program.
    let home = me.with_file_name(".browser");
    let mut command;
    if env::var_os(AS_BROWSER).is_some() {
        command = Command::new(home.join("chromium/chromium"));
        command
            .env_remove(AS_BROWSER)
            // In place of the image's own: Chromium's libraries are glibc's, built together.
            .env("LD_LIBRARY_PATH", home.join("lib"))
            .env("FONTCONFIG_FILE", home.join("fonts/fonts.conf"));
    } else {
        command = Command::new(home.join("agent-browser"));
        if env::var_os("AGENT_BROWSER_EXECUTABLE_PATH").is_none() {
            command
                .env("AGENT_BROWSER_EXECUTABLE_PATH", &me)
                .env(AS_BROWSER, "1");
        }
        if env::var_os("AGENT_BROWSER_SKILLS_DIR").is_none() {
            command.env("AGENT_BROWSER_SKILLS_DIR", home.join("skill-data"));
        }
        // The check compares Chrome's version with Google's release blog, over a network a
        // sandbox may not have, about a Chromium that is pinned here.
        if env::var_os("AGENT_BROWSER_NO_VERSION_CHECK").is_none() {
            command.env("AGENT_BROWSER_NO_VERSION_CHECK", "1");
        }
        // Named, so that where a session's `.pid` is is known here.
        let sockets = env::var_os("AGENT_BROWSER_SOCKET_DIR")
            .map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(|home| Path::new(&home).join(".agent-browser")));
        if let Some(sockets) = sockets {
            command.env("AGENT_BROWSER_SOCKET_DIR", &sockets);
            let args: Vec<OsString> = env::args_os().skip(1).collect();
            let session = session(&args);
            if gone(&sockets.join(format!("{session}.pid"))) {
                eprintln!(
                    "browser: the browser of session {session} has ended, with the machine \
                     parked or on its own: its pages, tabs, cookies and refs are gone, and \
                     `browser open` starts it again"
                );
            }
        }
    }
    let error = command.args(env::args_os().skip(1)).exec();
    eprintln!(
        "browser: running {}: {error}",
        command.get_program().to_string_lossy()
    );
    std::process::exit(127);
}

/// The session `args` name, as agent-browser reads it: `--session`, else
/// `AGENT_BROWSER_SESSION`, else `default`.
fn session(args: &[OsString]) -> String {
    let mut named = None;
    for (i, arg) in args.iter().enumerate() {
        let arg = arg.to_string_lossy();
        if let Some(name) = arg.strip_prefix("--session=") {
            named = Some(name.to_owned());
        } else if arg == "--session" {
            named = args
                .get(i + 1)
                .map(|name| name.to_string_lossy().into_owned());
        }
    }
    named
        .or_else(|| env::var("AGENT_BROWSER_SESSION").ok())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "default".into())
}

/// Whether the daemon `pid` names is gone, though the file is still there: written before the
/// machine last booted, as a parked machine boots again with none of its processes, or naming a
/// process that is not running.
fn gone(pid: &Path) -> bool {
    let Ok(written) = fs::metadata(pid).and_then(|about| about.modified()) else {
        return false;
    };
    let booted = fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|uptime| uptime.split_whitespace().next()?.parse::<f64>().ok())
        .and_then(|up| SystemTime::now().checked_sub(Duration::from_secs_f64(up)));
    let running = fs::read_to_string(pid)
        .ok()
        .and_then(|text| text.trim().parse::<u32>().ok())
        .is_some_and(|pid| Path::new(&format!("/proc/{pid}")).exists());
    booted.is_some_and(|booted| written < booted) || !running
}
