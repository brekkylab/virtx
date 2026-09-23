//! What a unix host has: one file, listing the resolvers every process on this machine
//! reads when it starts.
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
/// A file that is not there and a file that names none are the same answer -- none --
/// and what that means for a session is [`resolver`](super)'s to say rather than this
/// file's.
pub(crate) fn resolvers() -> Vec<Ipv4Addr> {
    std::fs::read_to_string(RESOLV_CONF)
        .map(|conf| nameservers(&conf))
        .unwrap_or_default()
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
