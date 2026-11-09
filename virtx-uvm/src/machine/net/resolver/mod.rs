//! Where the guest's name lookups actually go.
//!
//! # Not a resolver
//!
//! Nothing here resolves anything. The guest is told [`NAMESERVER`](super::NAMESERVER) is its
//! nameserver, and a query to it is sent, unread, to whatever this host asks -- and the answer
//! comes back the same way. So the guest gets the same names this machine gets, including
//! whatever a VPN or a corporate resolver is serving, without any of it being configured
//! twice.
//!
//! # Read once, at the start
//!
//! The host is asked when the machine starts and not again. A host whose resolver changes
//! mid-run -- a VPN coming up, a laptop changing networks -- leaves a machine pointed
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
//! Either way the answer is IPv4 addresses and only those: the nameserver the guest is told
//! of is one, and a query to it is carried to one.

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

use std::net::Ipv4Addr;

/// The host's resolvers, in whatever order the host gives them.
///
/// Empty is a real answer and not a failure: a host with no resolver configured is one
/// whose guest cannot look up names either, and that is a network which works by address
/// rather than one that failed to start. It is said once, on stderr, because a machine
/// whose names silently do not resolve is the hardest kind of broken to read from inside
/// the guest.
pub(crate) fn host_resolvers() -> Vec<Ipv4Addr> {
    #[cfg(unix)]
    let resolvers = unix::resolvers();
    #[cfg(windows)]
    let resolvers = windows::resolvers();

    if resolvers.is_empty() {
        eprintln!(
            "virtx-uvm: this host names no IPv4 resolver; the machine's names will not resolve"
        );
    }
    resolvers
}
