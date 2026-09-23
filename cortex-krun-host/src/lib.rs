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
//!
//! [`CAPABILITIES_ARG`] and [`GPU_CAPABILITY`] are the exception that shows the rule. They are
//! the one exchange where a disagreement is *not* refused by anybody -- a server that asks in
//! words the boot does not answer in hears nothing back and concludes the boot cannot do the
//! thing -- so they are read from here by both ends.

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

/// The one argument a boot answers without becoming a machine: what this build of it can
/// make, one name to a line on stdout, and then it exits.
///
/// **Here rather than spelled at each end, unlike the rest of the command line.** An argument
/// either end gets wrong is refused by name, which is what makes the rest safe to write twice;
/// a capability that is misspelled is not refused by anybody -- the server hears a name it was
/// not looking for, concludes the boot cannot do the thing, and refuses a session the boot
/// would have taken. A word whose mismatch is silent is a word both ends have to read from one
/// place.
///
/// Asked at all because a capability is a fact about *that file*: the two binaries are built
/// by one `cargo build` but a cargo feature is not, and
/// `CORTEX_KRUN_BOOT_BIN` can name a boot from another build entirely.
pub const CAPABILITIES_ARG: &str = "--capabilities";

/// A boot that can attach a virtio-gpu, which is what `--gpu` needs and what the `gpu`
/// feature builds in.
pub const GPU_CAPABILITY: &str = "gpu";

/// What the guest execs: the agent, at the one path both ends spell.
pub fn agent_program() -> String {
    format!("{AGENT_PATH}/{AGENT_BIN}")
}

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
