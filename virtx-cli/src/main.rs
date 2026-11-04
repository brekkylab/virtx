//! `virtx` as a binary of its own; the command is `virtx_core::cli`.

use std::process::ExitCode;

fn main() -> ExitCode {
    ExitCode::from(virtx_core::cli::main(std::env::args_os()))
}
