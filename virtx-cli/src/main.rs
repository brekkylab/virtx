//! `virtx`, as a binary: `virtx_core::cli`, which manages a server's images and signs in to its
//! provider, the same command the Python and Node packages run through `libvirtx_core`.

fn main() -> std::process::ExitCode {
    std::process::ExitCode::from(virtx_core::cli::main(std::env::args_os()))
}
