//! `browser`: vercel-labs/agent-browser, driving the Chrome for Testing installed with it.
//!
//! Started by a caller, this runs agent-browser with its arguments, naming this program as the
//! browser to launch. Started by agent-browser as that browser, it runs Chrome with the
//! libraries and fonts installed for it, which nothing else agent-browser starts is given.
//! `build.rs` says what is installed and why.

use std::{env, os::unix::process::CommandExt as _, process::Command};

/// Set for agent-browser, and so for the browser it launches: that this program is started as
/// the browser.
const AS_BROWSER: &str = "VIRTX_BROWSER_AS_BROWSER";

fn main() {
    let me = env::current_exe().expect("/proc/self/exe names this program");
    // What `build.rs` installs, in `.agent-browser/` beside this program.
    let home = me.with_file_name(".agent-browser");
    let mut command;
    if env::var_os(AS_BROWSER).is_some() {
        command = Command::new(home.join("chrome/chrome"));
        command
            .env_remove(AS_BROWSER)
            // In place of the image's own: Chrome's libraries are glibc's, built together.
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
        // sandbox may not have, about a Chrome that is pinned here.
        if env::var_os("AGENT_BROWSER_NO_VERSION_CHECK").is_none() {
            command.env("AGENT_BROWSER_NO_VERSION_CHECK", "1");
        }
    }
    let error = command.args(env::args_os().skip(1)).exec();
    eprintln!(
        "browser: running {}: {error}",
        command.get_program().to_string_lossy()
    );
    std::process::exit(127);
}
