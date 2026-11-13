//! `virtx`: the core library's command (`virtx_core::cli`) as a binary of its own -- a virtx
//! server's images built and managed from a shell, and virtx cloud signed in to for the SDKs to
//! use (`login`, `token`, `whoami`, `logout`). The Python and Node packages ship the same command
//! in `libvirtx_core`.

fn main() -> std::process::ExitCode {
    std::process::ExitCode::from(virtx_core::cli::main(std::env::args_os()))
}
