//! `virtx`: build, list, tag, move and remove a virtx server's images from a shell.
//!
//! The spelling follows `docker`'s, in both of its forms: `virtx image <command>`, and the
//! top-level `build`, `images`, `inspect`, `rmi`, `pull`, `push`, `tag`, `save` and `load`,
//! which are the same commands; `prune` is `image prune` alone, as in `docker`.
//!
//! [`main`] runs it for the `virtx-cli` binary, and `virtx_cli_main` for the Python and Node
//! packages' `virtx`, which ship it in `libvirtx_core` rather than as a binary of its own.
//!
//! `command.rs` is the command tree and how it runs, `server.rs` how the server is reached,
//! `job.rs` and `transfer.rs` how a job or a transfer is followed, and `name.rs` how an argument
//! names an image; each command's arguments and handler are in the file of its name.

mod build;
mod command;
mod inspect;
mod job;
mod load;
mod ls;
mod name;
mod prompt;
mod prune;
mod pull;
mod push;
mod registry;
mod rm;
mod save;
mod server;
mod tag;
mod transfer;

use std::ffi::OsString;

/// Run `virtx` with `argv`, its program name first, and answer its exit code.
pub fn main(argv: impl IntoIterator<Item = impl Into<OsString> + Clone>) -> u8 {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("virtx: starting a runtime: {error}");
            return 1;
        }
    };
    runtime.block_on(command::run(argv))
}

#[cfg(feature = "ffi")]
mod ffi {
    use std::ffi::{CStr, OsString, c_char, c_int};

    /// Run `virtx` with the `argc` arguments in `argv`, its program name first, and answer its
    /// exit code. It blocks until the command ends, and takes the process's Ctrl-C meanwhile.
    ///
    /// # Safety
    ///
    /// Each of the `argc` pointers in `argv` is NUL-terminated.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn virtx_cli_main(argv: *const *const c_char, argc: usize) -> c_int {
        let argv: Vec<OsString> = (0..argc)
            .map(|i| os_string(unsafe { CStr::from_ptr(*argv.add(i)) }))
            .collect();
        // The command's Ctrl-C handler chains to the one it replaces, and the host's may not
        // return: Node's re-raises SIGINT, so the job would never be cancelled.
        #[cfg(unix)]
        // SAFETY: `signal` is process-wide and may be called from any thread at any time.
        unsafe {
            libc::signal(libc::SIGINT, libc::SIG_DFL);
        }
        // Unwinding into the host aborts it; a panic ends the command with 101, as in the binary.
        std::panic::catch_unwind(|| super::main(argv)).map_or(101, c_int::from)
    }

    // An argument is the bytes the OS gave, which on Unix need not be UTF-8.
    #[cfg(unix)]
    fn os_string(arg: &CStr) -> OsString {
        use std::os::unix::ffi::OsStringExt as _;
        OsString::from_vec(arg.to_bytes().to_vec())
    }

    #[cfg(not(unix))]
    fn os_string(arg: &CStr) -> OsString {
        arg.to_string_lossy().into_owned().into()
    }
}
