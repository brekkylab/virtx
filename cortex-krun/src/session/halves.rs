//! The other two halves, which are files beside this binary rather than parts of it.
//!
//! The boot is a member of this workspace, so cargo links it into the same directory as this
//! binary; the guest is cross-compiled by `build.rs` and left there too. So the three ship as
//! a set and a lookup is `current_exe`'s parent plus a name. Nothing is searched for on
//! `PATH`: a server that found *some* `cortex-krun-host` would be a server running a
//! different build's hypervisor against this build's command line, and that command line is
//! something the two ends have to agree on exactly.
//!
//! # Why they are separate binaries at all
//!
//! - **The boot has to be signed.** Creating a VM through Hypervisor.framework needs the
//!   `com.apple.security.hypervisor` entitlement, and libkrun `dlopen`s libkrunfw, which
//!   needs library validation off. Both are carried by a code signature, and a signature is
//!   read at `exec` -- so the process that creates a VM can never be the process that decided
//!   to. The signature goes on where the boot is linked: `scripts/macos-link.sh` is the cargo
//!   linker for this target and signs that one binary, so a plain `cargo build` produces an
//!   entitled boot rather than one that only works under `cargo run`.
//! - **The boot does not come back.** Entering the VMM hands it the process, and the VMM
//!   exits when the guest goes down. A server that entered one would stop being able to
//!   answer anything.
//! - **The guest is not this platform.** It is a static musl ELF that runs inside the VM.
//!   There is no arrangement in which those bytes are code this binary could call.
//!
//! # The overrides
//!
//! Each half can be named outright, which is what a caller who builds one some other way --
//! or ships the three somewhere with a layout of its own -- has instead of this rule.
//!
//! `CORTEX_KRUN_GUEST_BIN` is read twice and means two things. Here it names the file to copy
//! into a machine's share; in `build.rs` it names a file to place beside this binary instead
//! of cross-compiling one, which is a build this crate performs and can therefore be told to
//! skip. `CORTEX_KRUN_BOOT_BIN` is read only here, because there is no matching build to
//! skip: the boot is a workspace member and cargo links it beside this binary itself.
//!
//! A binary named by either is taken as it is. The boot's entitlement is the case that shows
//! what that costs -- `scripts/macos-link.sh` signs what it links and nothing else, so a boot
//! built elsewhere carries whatever signature its own build gave it, and one without the
//! entitlement fails at `hv_vm_create` rather than here.

use std::path::PathBuf;

/// The program a boot runs: `$CORTEX_KRUN_BOOT_BIN`, else the binary beside this one.
pub fn boot() -> anyhow::Result<PathBuf> {
    beside("CORTEX_KRUN_BOOT_BIN", "cortex-krun-host")
}

/// The program the guest execs: `$CORTEX_KRUN_GUEST_BIN`, else the binary beside this one.
/// Copied into every machine's share -- see [`Machine`](super::machine::Machine).
pub fn guest() -> anyhow::Result<PathBuf> {
    beside("CORTEX_KRUN_GUEST_BIN", "cortex-krun-guest")
}

/// One half, by the variable naming it or by its name next to this binary.
///
/// Both cases are checked rather than returned on trust: this path is about to be spawned or
/// copied into a guest's share, and a missing file reported here says which half and where it
/// was looked for, where the same file missing a moment later is a boot that failed to start.
fn beside(env: &str, name: &str) -> anyhow::Result<PathBuf> {
    if let Some(named) = std::env::var_os(env) {
        let path = PathBuf::from(named);
        anyhow::ensure!(path.is_file(), "{env}={} is not a file", path.display());
        return Ok(path);
    }

    // Resolved, not as invoked: a symlink on `PATH` says nothing about where the rest of the
    // set is, and the real file is what cargo put them next to.
    let exe =
        std::env::current_exe().map_err(|e| anyhow::anyhow!("asking where this binary is: {e}"))?;
    let path = exe
        .parent()
        .ok_or_else(|| anyhow::anyhow!("{} has no directory to look in", exe.display()))?
        .join(name);
    anyhow::ensure!(
        path.is_file(),
        "no {name} beside {} -- `cargo build --workspace` builds it there (a `-p cortex-krun` \
         builds only the guest, which is this crate's build script's), or name one in {env}",
        exe.display()
    );
    Ok(path)
}
