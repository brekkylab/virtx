//! The names the three halves of this console server share.
//!
//! The library half of `cortex-krun-host`. It links no hypervisor and knows nothing about
//! libkrun: everything here is a name that has to mean the same thing in more than one
//! process, and nothing here does anything.
//!
//! Which is why it is a library half of the boot rather than a package of its own: two of
//! these names are the boot's own answer to where it puts things, and the third party that
//! needs them -- `cortex-krun-guest` -- takes this package with no features and so gets none
//! of the binary's graph. `cortex-krun` takes it for the two tree paths it answers `init`
//! with.
//!
//! # What is deliberately not here
//!
//! The boot's command line. It is spelled where a boot is spawned and read where one starts,
//! and the two are one repository built by one `cargo build` -- so what a shared type would
//! buy is a compile error in place of a refusal, and the refusal is not a quiet one: a boot
//! given an argument it does not know says so and exits before a hypervisor is touched.

/// The virtio-console port carrying the console session.
///
/// The guest's *console* is the kernel's -- it is where a panic and every `printk` land -- so
/// the protocol runs on a named port beside it rather than on the console itself, which a
/// single kernel warning would corrupt.
pub const PORT_NAME: &str = "cortex-console";

/// Sent by the guest, once, as soon as it has the port open, and consumed by the server
/// before the first frame goes out.
///
/// A virtio-console port **discards** what the host writes while no process in the guest has
/// it open, rather than queueing it. So a server cannot put `init` on the wire as soon as the
/// boot connects -- the socket is up long before the kernel is -- and this is the guest saying
/// that something is reading.
pub const HANDSHAKE: &[u8; 8] = b"CORTEXKR";

/// Where the machine's own directory is mounted inside the guest, and therefore where the
/// agent binary is found.
///
/// A share rather than anything built into the image: the guest agent is this build's, and
/// the image is whoever's the session named.
///
/// A dotted name at the root, so that it is out of the way of whatever the image put there.
pub const AGENT_PATH: &str = "/.cortex";

/// The agent's file name inside [`AGENT_PATH`], which is also what the server copies into the
/// directory it shares.
pub const AGENT_BIN: &str = "cortex-krun-guest";

/// What the guest execs: the agent, at the one path both ends spell.
pub fn agent_program() -> String {
    format!("{AGENT_PATH}/{AGENT_BIN}")
}

/// Where the context is mounted inside the guest.
///
/// **A name for the role and not for the directory behind it.** What a session is given is a
/// directory somewhere on the host, and what it is *called* in here says which of the two
/// trees it is -- so a command reads `/context/src/main.rs` and a client sends the same
/// string, in a session that would have spelled it `/Users/someone/project/src/main.rs` on
/// the other side of the hypervisor and in another one that would have spelled it `/srv/wt/9`.
///
/// Three consequences, and all three are the reason:
///
/// - **A guest's paths do not name the host.** Where a caller keeps its projects is that
///   caller's, and a path is the easiest thing in the world to leak -- into a command's
///   output, a log, a build artifact, a model's transcript.
/// - **Two sessions over different directories are the same session to look at**, which is
///   what makes a transcript comparable and a command reproducible.
/// - **The guest's root stays a root.** Mounting at the host's own path means a guest with
///   `/Users/someone/project` in it, which is a directory tree invented inside an image to
///   mirror a stranger's laptop.
///
/// What it costs is that a mount point has two names, one per side -- and nothing has to
/// translate between them, because the protocol only ever speaks this one.
pub const CONTEXT_PATH: &str = "/context";

/// Where the artifacts tree is mounted inside the guest -- see [`CONTEXT_PATH`].
pub const ARTIFACTS_PATH: &str = "/artifacts";

#[cfg(test)]
mod tests {
    use super::*;

    /// One path, spelled once. The server copies the agent into the directory it shares, the
    /// boot shares that directory at [`AGENT_PATH`], and the guest is told to exec the join
    /// of the two -- three processes, and the machine execs nothing if they disagree.
    #[test]
    fn the_agent_is_at_one_path() {
        assert_eq!(agent_program(), format!("{AGENT_PATH}/{AGENT_BIN}"));
        assert_eq!(agent_program(), "/.cortex/cortex-krun-guest");
    }
}
