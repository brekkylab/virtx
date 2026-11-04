//! `virtx`, as a binary of its own: the command is `virtx_core::cli`, which `libvirtx_core`
//! also carries for the Python and Node packages.

use std::process::ExitCode;

fn main() -> ExitCode {
    ExitCode::from(virtx_core::cli::main(std::env::args_os()))
}
