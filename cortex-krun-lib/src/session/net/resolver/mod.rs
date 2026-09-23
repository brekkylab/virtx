//! Where the guest's name lookups actually go.
//!
//! # Not a resolver
//!
//! Nothing here resolves anything. The guest is told the gateway is its nameserver, and
//! what the gateway does with a query is send it, unread, to whatever this host asks --
//! and send the answer back the same way. So the guest gets the same names this machine
//! gets, including whatever a VPN or a corporate resolver is serving, without any of it
//! being configured twice.
//!
//! **A query is forwarded, not inspected.** That means a name is a way of saying
//! something outward even for a session whose packets reach nothing -- see
//! [`Network::HostOnly`](super::Network::HostOnly), which says so where a caller choosing
//! a level will read it.
//!
//! # Read once, at the start
//!
//! The host is asked when the stack starts and not again. A host whose resolver changes
//! mid-session -- a VPN coming up, a laptop changing networks -- leaves a session pointed
//! at the old one, which is the same thing that happens to every process on this machine
//! that read its resolvers when it started.
//!
//! # What each platform is asked
//!
//! ```text
//!   unix     /etc/resolv.conf        the file every process there reads
//!   windows  GetAdaptersAddresses    per-adapter, since there is no such file
//! ```
//!
//! Either way the answer is IPv4 addresses and only those: this stack carries no v6, so a
//! resolver it could not send a packet to is not one it can offer the guest.

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

use std::net::Ipv4Addr;

/// The host's resolvers, in whatever order the host gives them.
///
/// Empty is a real answer and not a failure: a host with no resolver configured is one
/// whose guest cannot look up names either, and that is a network which works by address
/// rather than one that failed to start. It is said once, on stderr, because a session
/// whose names silently do not resolve is the hardest kind of broken to read from inside
/// the guest.
pub(crate) fn host_resolvers() -> Vec<Ipv4Addr> {
    #[cfg(unix)]
    let resolvers = unix::resolvers();
    #[cfg(windows)]
    let resolvers = windows::resolvers();

    if resolvers.is_empty() {
        eprintln!(
            "cortex-krun: this host names no IPv4 resolver; the session's names will not resolve"
        );
    }
    resolvers
}
