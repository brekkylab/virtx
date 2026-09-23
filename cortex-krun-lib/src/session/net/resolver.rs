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
//! `/etc/resolv.conf` is read when the stack starts and not again. A host whose resolver
//! changes mid-session -- a VPN coming up, a laptop changing networks -- leaves a session
//! pointed at the old one, which is the same thing that happens to every process on this
//! machine that read the file when it started.
//!
//! # macOS, and what this file is not
//!
//! On macOS the authority is `scutil`'s configuration, not this file: a resolver scoped
//! to one domain, or to one interface, lives only there. `/etc/resolv.conf` carries the
//! default resolvers, which is what a session wants and all it is given. A name that only
//! resolves through a scoped resolver does not resolve in a guest.

use std::net::Ipv4Addr;

/// Where every Unix has kept it, and where macOS keeps the default view of it.
const RESOLV_CONF: &str = "/etc/resolv.conf";

/// The host's resolvers, in the order the file names them.
///
/// Empty is a real answer and not a failure: a host with no resolver configured is one
/// whose guest cannot look up names either, and that is a network which works by address
/// rather than one that failed to start. The stack says so once, on stderr, because a
/// session whose names silently do not resolve is the hardest kind of broken to read from
/// inside the guest.
pub(crate) fn host_resolvers() -> Vec<Ipv4Addr> {
    let Ok(conf) = std::fs::read_to_string(RESOLV_CONF) else {
        eprintln!(
            "cortex-krun: {RESOLV_CONF} could not be read; the session's names will not resolve"
        );
        return Vec::new();
    };

    let resolvers = nameservers(&conf);
    if resolvers.is_empty() {
        eprintln!(
            "cortex-krun: {RESOLV_CONF} names no IPv4 resolver; the session's names will not resolve"
        );
    }
    resolvers
}

/// The `nameserver` lines of a `resolv.conf`, as far as they are IPv4 addresses.
///
/// IPv6 resolvers are passed over rather than refused: this stack carries no v6, so an
/// address it cannot send a packet to is not one it can offer the guest. A file that
/// names both ends up with the v4 ones, which is the working subset of what the host has.
fn nameservers(conf: &str) -> Vec<Ipv4Addr> {
    conf.lines()
        .map(|line| line.split('#').next().unwrap_or("").trim())
        .filter_map(|line| line.strip_prefix("nameserver"))
        .filter_map(|rest| rest.trim().parse().ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_nameservers_are_taken_in_order_and_nothing_else_is() {
        let conf = "\
# a comment
search example.com
nameserver 192.168.1.1
options edns0
nameserver 9.9.9.9
";
        assert_eq!(
            nameservers(conf),
            [Ipv4Addr::new(192, 168, 1, 1), Ipv4Addr::new(9, 9, 9, 9)]
        );
    }

    /// A v6 resolver is a resolver this stack has no way to reach, so it is passed over
    /// rather than turned into a lookup that never answers.
    #[test]
    fn a_resolver_this_stack_cannot_reach_is_passed_over() {
        let conf = "nameserver fe80::1\nnameserver 1.1.1.1\n";
        assert_eq!(nameservers(conf), [Ipv4Addr::new(1, 1, 1, 1)]);
    }

    #[test]
    fn a_file_that_names_none_yields_none() {
        assert!(nameservers("search example.com\n").is_empty());
        assert!(nameservers("").is_empty());
    }
}
