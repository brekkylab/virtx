//! Where a guest's flow goes, asked once per flow: for a TCP connection at its SYN, for UDP at
//! the first datagram between two ends.
//!
//! A machine with a network reaches what the internet routes, on v4 and v6, and the nameserver
//! it is told of -- at either of its addresses -- is this host's resolver, for DNS alone: it is how the guest's names resolve at all. Nothing
//! else of this host's is the guest's to reach -- its loopback, its private network, its
//! cloud's metadata. A machine without reaches nothing.
//!
//! Its client's ports, at the gateway's address, it reaches with or without a network, through
//! the sandbox's relay: a connection there opens with the relay's greeting.
//!
//! What goes nowhere: a SYN is reset, as a port nothing listens on is, and a datagram is
//! dropped.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use super::{LAN, resolver};
use crate::machine::ClientRelay;

/// The DNS port, the one the nameserver answers on.
const DNS_PORT: u16 = 53;

pub(super) struct Egress {
    pub(super) enabled: bool,
    /// Where a query to the nameserver goes, and `None` for a host that names no resolver --
    /// whose guest then resolves no names, which is what every other process there gets.
    pub(super) resolver: Option<SocketAddr>,
    /// Where a connection to one of its client's ports goes.
    pub(super) client: Option<ClientRelay>,
}

/// Where a guest's TCP connection goes on this host, and what is sent there first.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Route {
    pub(super) at: SocketAddr,
    /// What the host socket is written before the guest's bytes, and answered with a verdict
    /// before the guest's SYN is: empty for a connection carried as it is.
    pub(super) greeting: Vec<u8>,
}

impl Egress {
    pub(super) fn new(enabled: bool, client: Option<ClientRelay>) -> Egress {
        let resolver = match enabled {
            true => resolver::host_resolvers().first().copied(),
            false => None,
        };
        Egress {
            enabled,
            resolver: resolver.map(|ip| SocketAddr::new(ip.into(), DNS_PORT)),
            client,
        }
    }

    /// Where a TCP connection the guest opened to `to` goes on this host, or `None` for
    /// nowhere: one of its client's ports to the relay, whatever `enabled` is, and the rest as
    /// [`route`](Self::route) says.
    pub(super) fn connect(&self, to: SocketAddr) -> Option<Route> {
        if to.ip() == LAN.gateway {
            let relay = self.client.as_ref()?;
            return relay.ports.contains(&to.port()).then(|| Route {
                at: relay.address,
                greeting: relay.greeting(to.port()),
            });
        }
        self.route(to).map(|at| Route {
            at,
            greeting: Vec::new(),
        })
    }

    /// Where a flow the guest sent to `to` goes on this host, or `None` for nowhere.
    pub(super) fn route(&self, to: SocketAddr) -> Option<SocketAddr> {
        if !self.enabled {
            return None;
        }
        if to.ip() == LAN.nameserver || to.ip() == LAN.nameserver_v6 {
            return self.resolver.filter(|_| to.port() == DNS_PORT);
        }
        routed(to.ip()).then_some(to)
    }
}

/// Whether `to` is the nameserver's DNS, at either of its addresses.
pub(super) fn is_dns(to: SocketAddr) -> bool {
    to.port() == DNS_PORT && (to.ip() == LAN.nameserver || to.ip() == LAN.nameserver_v6)
}

/// Whether the internet routes `ip`, taken canonical: an address no network but this
/// host's own reaches is not, and neither is one every network reaches somewhere else.
///
/// This function is `pub(crate)` to give the image registry proxy the same destination
/// check for connections it opens on the host.
pub(crate) fn routed(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [a, b, c, _] = ip.octets();
            !(ip.is_unspecified()
                || ip.is_loopback()
                || ip.is_private()
                || ip.is_link_local()
                || ip.is_broadcast()
                || ip.is_multicast()
                || ip.is_documentation()
                // "This network", carrier-grade NAT, IETF protocol assignments, benchmarking,
                // and the reserved block above multicast.
                || a == 0
                || (a == 100 && b & 0xc0 == 64)
                || (a == 192 && b == 0 && c == 0)
                || (a == 198 && b & 0xfe == 18)
                || a >= 240)
        }
        IpAddr::V6(ip) => {
            let s = ip.segments();
            // The two that carry a v4 address carry it for the v4 internet, and are routed
            // where it is: NAT64's, which a v6-only host's resolver answers with, and 6to4's.
            let embedded = |high: u16, low: u16| {
                routed(Ipv4Addr::from(((high as u32) << 16) | low as u32).into())
            };
            if s[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
                return embedded(s[6], s[7]);
            }
            if s[0] == 0x2002 {
                return embedded(s[1], s[2]);
            }
            // Global unicast, which is all the v6 internet routes, less documentation's.
            s[0] & 0xe000 == 0x2000 && !(s[0] == 0x2001 && s[1] == 0x0db8)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::net::{Ipv6Addr, SocketAddrV4, SocketAddrV6};

    const PUBLIC: SocketAddr =
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(93, 184, 215, 14), 443));
    const PUBLIC_V6: SocketAddr = SocketAddr::V6(SocketAddrV6::new(
        Ipv6Addr::new(0x2606, 0x4700, 0, 0, 0, 0, 0, 0x1111),
        443,
        0,
        0,
    ));

    fn on() -> Egress {
        Egress {
            enabled: true,
            resolver: Some(SocketAddr::new(Ipv4Addr::new(192, 168, 1, 1).into(), 53)),
            client: None,
        }
    }

    /// A relay for the client's port 9222.
    fn relayed() -> ClientRelay {
        ClientRelay {
            address: "127.0.0.1:4000".parse().unwrap(),
            ports: vec![9222],
            key: "k".repeat(64),
        }
    }

    /// With a network, what the internet routes is reached where it was sent, on v4 and v6.
    #[test]
    fn a_network_reaches_what_the_internet_routes() {
        for to in [PUBLIC, PUBLIC_V6] {
            assert_eq!(on().route(to), Some(to));
        }
    }

    /// With a network, nothing of this host's or its network's is reached, the gateway included.
    #[test]
    fn a_network_reaches_nothing_the_internet_does_not_route() {
        let v4 = [
            LAN.gateway,
            Ipv4Addr::LOCALHOST,
            Ipv4Addr::new(10, 1, 2, 3),
            Ipv4Addr::new(172, 16, 0, 1),
            Ipv4Addr::new(192, 168, 1, 1),
            Ipv4Addr::new(169, 254, 169, 254),
            Ipv4Addr::new(100, 64, 0, 1),
            Ipv4Addr::new(224, 0, 0, 251),
            Ipv4Addr::BROADCAST,
            Ipv4Addr::UNSPECIFIED,
        ]
        .map(IpAddr::from);
        let v6 = [
            LAN.gateway_v6,
            LAN.guest_v6,
            Ipv6Addr::LOCALHOST,
            Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 1),
        ]
        .map(IpAddr::from);
        for ip in v4.into_iter().chain(v6) {
            assert_eq!(on().route(SocketAddr::new(ip, 80)), None, "{ip}");
        }
    }

    /// The nameserver, at either address, is this host's resolver, for DNS and nothing else; a
    /// host with none resolves nothing.
    #[test]
    fn the_nameserver_is_this_hosts_resolver() {
        let egress = on();
        let unnamed = Egress {
            resolver: None,
            ..on()
        };
        for ip in [IpAddr::from(LAN.nameserver), LAN.nameserver_v6.into()] {
            let dns = SocketAddr::new(ip, 53);
            assert_eq!(egress.route(dns), egress.resolver, "{ip}");
            assert_eq!(egress.route(SocketAddr::new(ip, 80)), None, "{ip}");
            assert_eq!(unnamed.route(dns), None, "{ip}");
        }
    }

    /// Without a network, nothing is reached, the nameserver included.
    #[test]
    fn no_network_reaches_nothing() {
        let off = Egress {
            enabled: false,
            ..on()
        };
        for to in [
            PUBLIC,
            PUBLIC_V6,
            SocketAddr::new(LAN.nameserver.into(), 53),
        ] {
            assert_eq!(off.route(to), None, "{to}");
        }
    }

    /// A client's port is the relay's, with its greeting, with or without a network; another
    /// port at the gateway, or any with no relay, goes nowhere.
    #[test]
    fn a_clients_port_at_the_gateway_goes_to_the_relay_alone() {
        let port = SocketAddr::new(LAN.gateway.into(), 9222);
        for enabled in [true, false] {
            let egress = Egress {
                enabled,
                client: Some(relayed()),
                ..on()
            };
            assert_eq!(
                egress.connect(port),
                Some(Route {
                    at: relayed().address,
                    greeting: relayed().greeting(9222),
                })
            );
            assert_eq!(
                egress.connect(SocketAddr::new(LAN.gateway.into(), 9223)),
                None
            );
            // Not a UDP flow's.
            assert_eq!(egress.route(port), None);
        }
        assert_eq!(on().connect(port), None);
        assert_eq!(
            on().connect(PUBLIC),
            Some(Route {
                at: PUBLIC,
                greeting: Vec::new()
            })
        );
    }

    /// What `routed` says of v6, which the registry proxy asks of the addresses a name resolves
    /// to.
    #[test]
    fn routed_v6_is_global_unicast_and_what_carries_a_routed_v4() {
        for (ip, is) in [
            ("2606:4700::1111", true),
            ("64:ff9b::5db8:d70e", true),
            ("::1", false),
            ("fd00::1", false),
            ("fe80::1", false),
            ("64:ff9b::a9fe:a9fe", false),
            ("2002:c0a8:0101::1", false),
            ("2001:db8::1", false),
        ] {
            assert_eq!(routed(ip.parse().unwrap()), is, "{ip}");
        }
    }
}
