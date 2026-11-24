//! Where a guest's socket may go, and the one place it goes somewhere else.
//!
//! # The decision is a socket, not a rule
//!
//! Everything the guest reaches, it reaches because libkrun opened a host socket for it,
//! and every such socket is opened after asking this. So a policy here is not a filter in
//! front of a network -- it is the answer to "where does this request get a socket", asked
//! once per `connect`, destination or `listen`. There is nothing in the guest that can route
//! around it, because the only way off the guest is the device that asks.
//!
//! # What it answers
//!
//! ```text
//!   the guest asks for   it gets
//!   ──────────────────────────────────────────────────────────────────────
//!   NAMESERVER:53        this host's resolver, with a network
//!   anywhere else        where it asked, with a network and if the internet
//!                        routes it
//!   listen               a listener in the guest, and nothing here
//! ```
//!
//! Whatever is not answered is refused: `ECONNREFUSED` for a `connect`, and a dropped
//! datagram for a `sendto`. A guest only asks here for what its own stack cannot reach, so its
//! own loopback, and every port listened on in it, never comes here.
//!
//! **Nothing of this host's is the guest's to reach**: a machine on a server others reach
//! would otherwise reach whatever the server does, which is its own loopback, its private
//! network and its cloud's metadata. The resolver is the one exception, and only for DNS: it
//! is how the guest's names resolve at all.
//!
//! **No `listen` is this host's**: the guest kernel, told no, listens on its own stack, where
//! the guest's programs reach it and a published port reaches it through the agent -- see
//! [`net`](super).

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use super::NAMESERVER;

/// The DNS port, the one [`NAMESERVER`] answers on.
const DNS_PORT: u16 = 53;

/// Everything a guest's request is judged by: whether it may go anywhere, and where names go.
pub(crate) struct Policy {
    /// Whether the guest's own connections reach the addresses the internet routes.
    network: bool,

    /// Where a query to [`NAMESERVER`] goes, and `None` for a machine with no network or a
    /// host that names no resolver -- whose guest then resolves no names.
    resolver: Option<Ipv4Addr>,
}

impl Policy {
    pub(crate) fn new(network: bool, resolver: Option<Ipv4Addr>) -> Self {
        Policy { network, resolver }
    }
}

impl krun::TsiPolicy for Policy {
    fn connect(&self, to: SocketAddr) -> Option<SocketAddr> {
        let ip = to.ip().to_canonical();
        if ip == IpAddr::V4(NAMESERVER) {
            return (self.network && to.port() == DNS_PORT)
                .then_some(self.resolver)
                .flatten()
                .map(|resolver| SocketAddr::new(resolver.into(), DNS_PORT));
        }
        (self.network && routed(ip)).then_some(to)
    }

    fn listen(&self, _at: SocketAddr) -> Option<SocketAddr> {
        None
    }
}

/// Whether the internet routes `ip`, taken canonical: an address no network but this
/// host's own reaches is not, and neither is one every network reaches somewhere else.
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
    use std::net::{Ipv6Addr, SocketAddrV4};

    use krun::TsiPolicy;

    use super::*;

    const PUBLIC: SocketAddr =
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(93, 184, 215, 14), 443));

    fn at(ip: impl Into<IpAddr>, port: u16) -> SocketAddr {
        SocketAddr::new(ip.into(), port)
    }

    /// The guest's resolver is this host's, for DNS and nothing else -- a private one too: it
    /// is how names resolve at all.
    #[test]
    fn the_nameserver_is_this_hosts_resolver() {
        let resolver = Ipv4Addr::new(192, 168, 1, 1);
        let policy = Policy::new(true, Some(resolver));
        assert_eq!(policy.connect(at(NAMESERVER, 53)), Some(at(resolver, 53)));
        assert_eq!(policy.connect(at(NAMESERVER, 80)), None);
        assert_eq!(Policy::new(true, None).connect(at(NAMESERVER, 53)), None);
    }

    /// Without a network nothing off the machine is reached.
    #[test]
    fn no_network_reaches_nothing() {
        let policy = Policy::new(false, None);
        assert_eq!(policy.connect(PUBLIC), None);
        assert_eq!(policy.connect(at(Ipv4Addr::LOCALHOST, 8080)), None);
    }

    /// A network reaches the internet and nothing of this host's or its network's: not its
    /// loopback, however it is spelled.
    #[test]
    fn a_network_reaches_only_what_the_internet_routes() {
        let policy = Policy::new(true, None);
        assert_eq!(policy.connect(PUBLIC), Some(PUBLIC));
        let v6 = at("2606:4700::1111".parse::<Ipv6Addr>().unwrap(), 443);
        assert_eq!(policy.connect(v6), Some(v6));
        for to in [
            at(Ipv4Addr::LOCALHOST, 8080),
            at(Ipv4Addr::new(127, 1, 2, 3), 22),
            at(Ipv4Addr::LOCALHOST.to_ipv6_mapped(), 8080),
            at(Ipv4Addr::UNSPECIFIED, 8080),
            at(Ipv4Addr::new(10, 1, 2, 3), 80),
            at(Ipv4Addr::new(172, 16, 0, 1), 80),
            at(Ipv4Addr::new(192, 168, 1, 1), 80),
            at(Ipv4Addr::new(169, 254, 169, 254), 80),
            at(Ipv4Addr::new(100, 64, 0, 1), 80),
            at(Ipv4Addr::new(224, 0, 0, 251), 5353),
            at(Ipv6Addr::LOCALHOST, 8080),
            at(Ipv4Addr::new(10, 0, 0, 1).to_ipv6_mapped(), 80),
            at("fd00::1".parse::<Ipv6Addr>().unwrap(), 80),
            at("fe80::1".parse::<Ipv6Addr>().unwrap(), 80),
            at("fd00:ec2::254".parse::<Ipv6Addr>().unwrap(), 80),
            at("64:ff9b::a9fe:a9fe".parse::<Ipv6Addr>().unwrap(), 80),
            at("2002:c0a8:0101::1".parse::<Ipv6Addr>().unwrap(), 80),
        ] {
            assert_eq!(policy.connect(to), None, "{to}");
        }
        let nat64 = at("64:ff9b::5db8:d70e".parse::<Ipv6Addr>().unwrap(), 443);
        assert_eq!(policy.connect(nat64), Some(nat64));
    }

    /// Every listen stays in the guest, with a network or without.
    #[test]
    fn no_listen_is_this_hosts() {
        for network in [true, false] {
            let policy = Policy::new(network, None);
            assert_eq!(policy.listen(at(Ipv4Addr::UNSPECIFIED, 80)), None);
            assert_eq!(policy.listen(at(Ipv6Addr::LOCALHOST, 5900)), None);
        }
    }
}
