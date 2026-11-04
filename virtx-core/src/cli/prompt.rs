//! Asking before what cannot be undone.

use std::io::IsTerminal as _;

use anyhow::Context as _;

/// Ask on stderr and read the answer from stdin: `y` is yes, and anything else, or no answer
/// at all, is no.
pub(crate) fn confirm(question: &str) -> anyhow::Result<bool> {
    eprint!("{question} [y/N] ");
    let mut answer = String::new();
    let read = std::io::stdin()
        .read_line(&mut answer)
        .context("reading an answer")?;
    // An answer that is not typed, or none at all, leaves the prompt's line open.
    if read == 0 || !std::io::stdin().is_terminal() {
        eprintln!();
    }
    Ok(matches!(answer.trim(), "y" | "Y" | "yes"))
}

/// `text` in bold when `on`, for a terminal that shows it: not through a pipe, and not under
/// `NO_COLOR`, which tools take as "no styling".
pub(crate) fn emphasis(text: &str, on: bool) -> String {
    let shown = std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none();
    if on && shown {
        format!("\x1b[1m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}
