//! Where a guest's flow goes, asked once per flow: for a TCP connection at its SYN, for UDP at
//! the first datagram between two ends.
//!
//! A machine with a network reaches what the internet routes, on v4 and v6, and the nameserver
//! it is told of -- at either of its addresses -- is this host's resolver, for DNS alone: it is how the guest's names resolve at all. Nothing
//! else of this host's is the guest's to reach -- its loopback, its private network, its
//! cloud's metadata. A machine without reaches nothing.
//!
//! Each flow to the internet is also put to the embedder's [`Outward`], where it set one, with
//! what the server gave this machine: one it refuses goes nowhere.
//!
//! What goes nowhere: a SYN is reset, as a port nothing listens on is, and a datagram is
//! dropped.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use crate::outward::Outward;

use super::{LAN, resolver};

/// The DNS port, the one the nameserver answers on.
const DNS_PORT: u16 = 53;

pub(super) struct Egress {
    pub(super) enabled: bool,
    /// Where a query to the nameserver goes, and `None` for a host that names no resolver --
    /// whose guest then resolves no names, which is what every other process there gets.
    pub(super) resolver: Option<SocketAddr>,
    /// The embedder's say on each flow to the internet, and what it was given for this
    /// machine.
    pub(super) outward: Option<(Arc<dyn Outward>, serde_json::Value)>,
}

impl Egress {
    pub(super) fn new(enabled: bool) -> Egress {
        let resolver = match enabled {
            true => resolver::host_resolvers().first().copied(),
            false => None,
        };
        Egress {
            enabled,
            resolver: resolver.map(|ip| SocketAddr::new(ip.into(), DNS_PORT)),
            outward: None,
        }
    }

    /// Each flow to the internet put to `outward` too, with `given`.
    pub(super) fn with_outward(
        mut self,
        outward: Arc<dyn Outward>,
        given: serde_json::Value,
    ) -> Egress {
        self.outward = Some((outward, given));
        self
    }

    /// Where a flow the guest sent to `to` goes on this host, or `None` for nowhere.
    pub(super) fn route(&self, to: SocketAddr) -> Option<SocketAddr> {
        if !self.enabled {
            return None;
        }
        if to.ip() == LAN.nameserver || to.ip() == LAN.nameserver_v6 {
            return self.resolver.filter(|_| to.port() == DNS_PORT);
        }
        let outward =
            || (self.outward.as_ref()).is_none_or(|(outward, given)| outward.outward(given, to));
        (routed(to.ip()) && outward()).then_some(to)
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
            outward: None,
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

    /// What an embedder's `Outward` was asked: what it was given, and where to.
    struct Asked(std::sync::Mutex<Vec<(serde_json::Value, SocketAddr)>>);

    impl Outward for Asked {
        fn outward(&self, given: &serde_json::Value, to: SocketAddr) -> bool {
            self.0.lock().unwrap().push((given.clone(), to));
            to.port() != 25
        }
    }

    /// With a network, the embedder says whether each flow to the internet goes, with what it
    /// was given; the nameserver's DNS and what the internet does not route are not put to it,
    /// nor anything without a network.
    #[test]
    fn the_embedder_decides_each_flow_to_the_internet_and_nothing_else() {
        let asked = Arc::new(Asked(std::sync::Mutex::new(Vec::new())));
        let given = serde_json::json!({ "sandbox": "a" });
        let egress = on().with_outward(asked.clone(), given.clone());
        let smtp = SocketAddr::new(Ipv4Addr::new(142, 250, 0, 26).into(), 25);
        assert_eq!(egress.route(PUBLIC), Some(PUBLIC));
        assert_eq!(egress.route(smtp), None);
        assert!(
            egress
                .route(SocketAddr::new(LAN.nameserver.into(), 53))
                .is_some()
        );
        assert_eq!(
            egress.route(SocketAddr::new(Ipv4Addr::new(10, 0, 0, 1).into(), 80)),
            None
        );
        assert_eq!(
            egress.route(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 8080)),
            None
        );
        assert_eq!(
            *asked.0.lock().unwrap(),
            [(given.clone(), PUBLIC), (given.clone(), smtp)]
        );

        let off = Egress {
            enabled: false,
            ..on()
        }
        .with_outward(asked.clone(), given);
        assert_eq!(off.route(PUBLIC), None);
        assert_eq!(
            asked.0.lock().unwrap().len(),
            2,
            "not asked without a network"
        );
    }
}
