//! How much of the network a session may reach, and the one place it is decided.
//!
//! # The decision is a socket, not a rule
//!
//! Everything the guest can reach, it reaches because this process opened a socket for
//! it. So a policy here is not a filter in front of a network -- it is the answer to
//! "does this flow get a socket at all", asked once per flow at the moment one is wanted.
//! There is no second path to fall back to, nothing in the guest that can route around
//! it, and no rule that can be out of date with respect to a connection already open:
//! a flow that was refused never had anything to be out of date with.
//!
//! # Why reaching the host is a separate grant
//!
//! Because the convenient way to say "let it talk to the host" is a level that includes
//! the host, and every such level also includes everything *else* on that side -- every
//! port on this machine, every service on the LAN it is on. Widening a session's outside
//! reach would then quietly widen its inside reach, which is the one direction a sandbox
//! must never widen by accident.
//!
//! So the host is not an address a level grants. It is [`Reach::Host`], reachable only
//! at a port [`Policy::host_ports`] names, and it stays exactly that at every level --
//! [`Network::Full`] included.
//!
//! # What the guest sees
//!
//! The gateway, and nothing else with an address. A guest that connects to
//! [`GATEWAY`](super::GATEWAY) is asking for this machine, and what it gets is whatever
//! the grant allows on loopback -- never an interface, never the LAN. Every other
//! destination is the outside, judged by its address alone.

use std::net::Ipv4Addr;

use super::{GATEWAY, Network};

/// The port the stack answers DNS on, at [`GATEWAY`]. Always reachable when there is a
/// device at all: a network whose names do not resolve is one that works only for a
/// caller who already knows every address, which is no one.
pub(crate) const DNS_PORT: u16 = 53;

/// What a destination turns out to be, once the address has been looked at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Reach {
    /// This machine, at a granted port, which the stack opens on loopback.
    Host,

    /// The stack's own resolver.
    Resolver,

    /// Somewhere else, and allowed.
    Outside,

    /// Somewhere else, and not.
    Refused,
}

/// A level and the grant that goes with it, which together are the whole of what a flow
/// is judged by.
#[derive(Clone, Debug, Default)]
pub(crate) struct Policy {
    pub(crate) network: Network,

    /// TCP ports on this machine the session may open, on loopback and nowhere else.
    /// Empty is the default, and an empty grant is no reach at all rather than a wide one
    /// -- a rule with no ports matching every port is exactly the mistake this is shaped
    /// to make impossible.
    pub(crate) host_ports: Vec<u16>,
}

impl Policy {
    /// What a flow to `addr:port` gets. UDP and TCP are judged the same way, but for the
    /// resolver, which is a UDP port of the stack's own and is never a host grant.
    pub(crate) fn reach(&self, addr: Ipv4Addr, port: u16, udp: bool) -> Reach {
        if addr == GATEWAY {
            return match (udp && port == DNS_PORT, self.host_ports.contains(&port)) {
                (true, _) => Reach::Resolver,
                (false, true) => Reach::Host,
                (false, false) => Reach::Refused,
            };
        }

        match self.network {
            // Never reached -- a disabled session has no device to send a frame on -- but
            // stated rather than left to a catch-all, so that adding a level cannot make
            // "no network" mean something by accident.
            Network::Disabled => Reach::Refused,
            Network::HostOnly => Reach::Refused,
            Network::Public if global(addr) => Reach::Outside,
            Network::Public => Reach::Refused,
            Network::Full => Reach::Outside,
        }
    }
}

/// Whether an address is one the internet routes to somebody.
///
/// Written out rather than taken from `Ipv4Addr::is_global`, which is unstable. The
/// ranges are the ones IANA marks as not globally reachable; what they have in common
/// for this module's purpose is that every one of them names something on the near side
/// of the sandbox boundary -- this machine, its LAN, its carrier's NAT -- rather than
/// something out on the network.
fn global(addr: Ipv4Addr) -> bool {
    let [a, b, ..] = addr.octets();

    !(addr.is_unspecified()
        || addr.is_loopback()
        || addr.is_private()
        || addr.is_link_local()
        || addr.is_broadcast()
        || addr.is_multicast()
        || addr.is_documentation()
        // 100.64.0.0/10, carrier-grade NAT: the address a home router has on the far
        // side, and so another name for "still inside somebody's network".
        || (a == 100 && (64..128).contains(&b))
        // 192.0.0.0/24, IETF protocol assignments.
        || (a == 192 && b == 0 && addr.octets()[2] == 0)
        // 198.18.0.0/15, benchmarking.
        || (a == 198 && (18..20).contains(&b))
        // 240.0.0.0/4, reserved.
        || a >= 240)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(network: Network) -> Policy {
        Policy {
            network,
            host_ports: Vec::new(),
        }
    }

    const PUBLIC: Ipv4Addr = Ipv4Addr::new(93, 184, 215, 14);

    #[test]
    fn a_level_decides_only_the_outside() {
        for (network, reach) in [
            (Network::HostOnly, Reach::Refused),
            (Network::Public, Reach::Outside),
            (Network::Full, Reach::Outside),
        ] {
            assert_eq!(at(network).reach(PUBLIC, 443, false), reach, "{network:?}");
        }
    }

    /// The whole point of the grant being a grant: the widest level still reaches nothing
    /// on this machine, and the narrowest one reaches what it was given.
    #[test]
    fn the_host_is_the_grant_and_never_the_level() {
        assert_eq!(
            at(Network::Full).reach(GATEWAY, 8080, false),
            Reach::Refused
        );

        let granted = Policy {
            network: Network::HostOnly,
            host_ports: vec![8080],
        };
        assert_eq!(granted.reach(GATEWAY, 8080, false), Reach::Host);
        assert_eq!(granted.reach(GATEWAY, 8081, false), Reach::Refused);
    }

    /// A resolver a session cannot reach is a session whose names do not resolve, so it
    /// is not something a grant has to remember to include.
    #[test]
    fn the_resolver_needs_no_grant_and_is_udp_only() {
        assert_eq!(
            at(Network::HostOnly).reach(GATEWAY, 53, true),
            Reach::Resolver
        );
        assert_eq!(
            at(Network::HostOnly).reach(GATEWAY, 53, false),
            Reach::Refused
        );
    }

    /// What `Public` is for: the internet, and not the things that merely look like it
    /// from inside a guest.
    #[test]
    fn public_is_the_internet_and_not_the_near_side_of_it() {
        let policy = at(Network::Public);
        for near in [
            Ipv4Addr::new(127, 0, 0, 1),
            Ipv4Addr::new(10, 1, 2, 3),
            Ipv4Addr::new(172, 16, 0, 1),
            Ipv4Addr::new(192, 168, 1, 1),
            Ipv4Addr::new(169, 254, 169, 254),
            Ipv4Addr::new(100, 64, 0, 1),
            Ipv4Addr::new(224, 0, 0, 1),
            Ipv4Addr::new(255, 255, 255, 255),
        ] {
            assert_eq!(policy.reach(near, 80, false), Reach::Refused, "{near}");
        }
        assert_eq!(policy.reach(PUBLIC, 80, false), Reach::Outside);
    }
}
