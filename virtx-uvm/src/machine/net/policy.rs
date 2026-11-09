//! Where a guest's socket may go, and the few places it goes somewhere else.
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
//!   the guest asks for          it gets                          under
//!   ─────────────────────────────────────────────────────────────────────────────────
//!   NAMESERVER:53               this host's resolver             Host, Public
//!   loopback, or 0.0.0.0        its own published listener on    any
//!                               that port, refused while there
//!                               is none
//!                               else that port on this host's    Host
//!                               loopback
//!   anywhere else               where it asked                   Host
//!                               where it asked, if the internet  Public
//!                               routes it
//!   listen on a published port  a listener on this host's        any
//!                               loopback
//!   listen on any other port    a listener in the guest, and     any
//!                               nothing here
//! ```
//!
//! Whatever is not answered is refused: `ECONNREFUSED` for a `connect`, and a dropped
//! datagram for a `sendto`.
//!
//! **Under [`Host`](Network::Host), this host's loopback is the guest's to reach**, as it is
//! every other process's here: a dev server, the person's own browser, another machine's
//! published port. A guest only falls through to here for loopback when its own stack had
//! nothing listening on that port, so `127.0.0.1:<port>` in the guest is first its own and
//! then this host's. So is everything else this host reaches, its LAN included.
//!
//! **Under [`Public`](Network::Public), none of that is**: a machine on a server others
//! reach would otherwise reach whatever the server does, which is its own loopback, its
//! private network and its cloud's metadata. The resolver is the one exception, and only
//! for DNS: it is how the guest's names resolve at all.
//!
//! **Its own published ports are the guest's under any of them**, [`None`](Network::None)
//! included: they are in the machine, and reaching them leaves nothing.
//!
//! # A published port, listened on
//! A guest's `listen` on a port that was published is carried to this host's loopback at a
//! port of this host's choosing, which [`relay`](super::relay) connects to. Kept per console
//! port while nothing else holds it, so a restarted guest program listens where it did
//! before; and on loopback, so only this host reaches it -- no more than the published port.
//!
//! Any other `listen` is kept in the guest, which the guest kernel does by itself when told
//! no -- so a guest program serving another guest program works whether or not anyone
//! published its port.

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener};
use std::sync::Mutex;

use super::{NAMESERVER, Network};

/// The DNS port, the one [`NAMESERVER`] answers on.
const DNS_PORT: u16 = 53;

/// Everything a guest's request is judged by: where it may go, where names go, and which
/// console ports are published.
pub(crate) struct Policy {
    network: Network,

    /// Where a query to [`NAMESERVER`] goes, and `None` for a host that names no resolver --
    /// whose guest then resolves no names, which is what every other process here gets.
    resolver: Option<Ipv4Addr>,

    /// The console ports a connection from this host may reach.
    published: HashSet<u16>,

    /// Where on this host's loopback each published console port is listened on, once the
    /// guest has listened on it -- in each family apart, because a program listens in both
    /// and sometimes twice in one, and a second listen in one family must not take the
    /// port away from the other.
    listening: Mutex<HashMap<u16, Listeners>>,

    /// Where each published console port's relay listens on this host -- see
    /// [`relay`](super::relay) -- for a guest reaching its own port over v4.
    relays: HashMap<u16, SocketAddr>,
}

/// Where on this host's loopback a guest's listeners on one console port are, in each
/// family: `127.0.0.1` for v4 and `::1` for v6.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Listeners {
    pub(crate) v4: Option<u16>,
    pub(crate) v6: Option<u16>,
}

impl Listeners {
    fn get(&self, family: IpAddr) -> Option<u16> {
        if family.is_ipv4() { self.v4 } else { self.v6 }
    }

    fn set(&mut self, family: IpAddr, port: u16) {
        if family.is_ipv4() {
            self.v4 = Some(port);
        } else {
            self.v6 = Some(port);
        }
    }
}

impl Policy {
    pub(crate) fn new(
        network: Network,
        resolver: Option<Ipv4Addr>,
        published: impl IntoIterator<Item = u16>,
    ) -> Self {
        Policy {
            network,
            resolver,
            published: published.into_iter().collect(),
            listening: Mutex::new(HashMap::new()),
            relays: HashMap::new(),
        }
    }

    /// Say where each published console port's relay listens, so a guest's own v4 `connect`
    /// to one goes through it. Only the v4 ones are kept: they are what a v4 socket reaches.
    pub(crate) fn with_relays(
        mut self,
        relays: impl IntoIterator<Item = (u16, SocketAddr)>,
    ) -> Self {
        self.relays = relays.into_iter().filter(|(_, at)| at.is_ipv4()).collect();
        self
    }

    /// Where on this host a connection reaches the guest's listeners on `console`, or `None`
    /// while nothing in the guest listens there for this host to reach.
    pub(crate) fn listener(&self, console: u16) -> Option<Listeners> {
        self.listening.lock().unwrap().get(&console).copied()
    }

    /// The loopback port `console` is listened on in `family`'s family: the previous one while
    /// nothing holds it, a fresh one otherwise. `None` is a host with no free port.
    fn listen_port(&self, console: u16, family: IpAddr) -> Option<u16> {
        let mut listening = self.listening.lock().unwrap();
        let here = loopback(family);
        // A held port is either our guest's live listener or someone else's; we can't
        // tell which, so it isn't handed out again.
        let known = listening.get(&console).copied().unwrap_or_default();
        let free = |port: &u16| TcpListener::bind((here, *port)).is_ok();
        // This family's port again, else the other family's number when it is free here, so
        // that a program listening in both is at one port in both.
        let kept = known
            .get(family)
            .filter(free)
            .or_else(|| known.v4.or(known.v6).filter(free));
        let port = match kept {
            Some(port) => port,
            None => TcpListener::bind((here, 0))
                .and_then(|probe| probe.local_addr())
                .ok()?
                .port(),
        };
        listening.entry(console).or_default().set(family, port);
        Some(port)
    }
}

impl krun::TsiPolicy for Policy {
    fn connect(&self, to: SocketAddr) -> Option<SocketAddr> {
        let ip = to.ip().to_canonical();
        if ip == IpAddr::V4(NAMESERVER) {
            return (self.network != Network::None && to.port() == DNS_PORT)
                .then_some(self.resolver)
                .flatten()
                .map(|resolver| SocketAddr::new(resolver.into(), DNS_PORT));
        }
        // What `0.0.0.0` means as a destination on Linux is this machine, so it is loopback
        // by another name.
        if ip.is_loopback() || ip.is_unspecified() {
            if let Some(listeners) = self.listener(to.port()) {
                // Over v4, through the relay, which tries the guest's listener in both
                // families: a program that listened on `::` alone takes v4 in the guest, but on
                // this host it has only a v6 listener, which a v4 socket cannot connect to.
                if ip.is_ipv4()
                    && let Some(relay) = self.relays.get(&to.port())
                {
                    return Some(*relay);
                }
                return listeners
                    .get(ip)
                    .map(|port| SocketAddr::new(loopback(ip), port));
            }
            // One of this machine's own ports that nothing in it listens on yet is refused, as
            // it is without a network. Not carried out to the door on this host that leads
            // back in, which would take the connection and close it: a program waiting for
            // its own server to come up would hear "closed" where it retries on "refused".
            if self.published.contains(&to.port()) || self.network != Network::Host {
                return None;
            }
            // `0.0.0.0` is this host's loopback in the socket's family; any other loopback
            // address is reached as asked.
            let host = if ip.is_unspecified() {
                loopback(ip)
            } else {
                ip
            };
            return Some(SocketAddr::new(host, to.port()));
        }
        match self.network {
            Network::None => None,
            Network::Host => Some(to),
            Network::Public => routed(ip).then_some(to),
        }
    }

    fn listen(&self, at: SocketAddr) -> Option<SocketAddr> {
        if !self.published.contains(&at.port()) {
            return None;
        }
        let port = self.listen_port(at.port(), at.ip())?;
        Some(SocketAddr::new(loopback(at.ip()), port))
    }
}

/// This host's loopback in the family of `ip`, which is the family the socket asking was
/// made in and so the only one it can be given.
fn loopback(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(_) => Ipv4Addr::LOCALHOST.into(),
        IpAddr::V6(_) => Ipv6Addr::LOCALHOST.into(),
    }
}

/// Whether the internet routes `ip`, taken canonical: an address no network but this
/// host's own reaches is not, and neither is one every network reaches somewhere else.
fn routed(ip: IpAddr) -> bool {
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
    use std::net::SocketAddrV4;

    use krun::TsiPolicy;

    use super::*;

    const PUBLIC: SocketAddr =
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(93, 184, 215, 14), 443));

    fn at(ip: impl Into<IpAddr>, port: u16) -> SocketAddr {
        SocketAddr::new(ip.into(), port)
    }

    fn policy(published: impl IntoIterator<Item = u16>) -> Policy {
        Policy::new(Network::Host, None, published)
    }

    /// On is what this host reaches: anywhere goes where it was asked.
    #[test]
    fn the_outside_is_where_it_was_asked_for() {
        let policy = policy([]);
        assert_eq!(policy.connect(PUBLIC), Some(PUBLIC));
        let lan = at(Ipv4Addr::new(192, 168, 1, 1), 80);
        assert_eq!(policy.connect(lan), Some(lan));
    }

    /// This host's loopback is reached, whatever the port and however it is spelled.
    #[test]
    fn this_hosts_loopback_is_reached() {
        let policy = policy([]);
        for to in [
            at(Ipv4Addr::LOCALHOST, 8080),
            at(Ipv4Addr::new(127, 1, 2, 3), 22),
            at(Ipv6Addr::LOCALHOST, 8080),
        ] {
            assert_eq!(policy.connect(to), Some(to), "{to}");
        }
        assert_eq!(
            policy.connect(at(Ipv4Addr::UNSPECIFIED, 8080)),
            Some(at(Ipv4Addr::LOCALHOST, 8080))
        );
        assert_eq!(
            policy.connect(at(Ipv4Addr::LOCALHOST.to_ipv6_mapped(), 8080)),
            Some(at(Ipv4Addr::LOCALHOST, 8080))
        );
    }

    /// The guest's resolver is this host's, for DNS and nothing else.
    #[test]
    fn the_nameserver_is_this_hosts_resolver() {
        let resolver = Ipv4Addr::new(192, 168, 1, 1);
        let policy = Policy::new(Network::Host, Some(resolver), []);
        assert_eq!(policy.connect(at(NAMESERVER, 53)), Some(at(resolver, 53)));
        assert_eq!(policy.connect(at(NAMESERVER, 80)), None);
        assert_eq!(
            Policy::new(Network::Host, None, []).connect(at(NAMESERVER, 53)),
            None
        );
        // A private resolver is still the guest's under `Public`, for DNS: it is how names
        // resolve at all.
        let public = Policy::new(Network::Public, Some(resolver), []);
        assert_eq!(public.connect(at(NAMESERVER, 53)), Some(at(resolver, 53)));
        let none = Policy::new(Network::None, Some(resolver), []);
        assert_eq!(none.connect(at(NAMESERVER, 53)), None);
    }

    /// `None` reaches nothing off the machine, but still its own published ports.
    #[test]
    fn none_reaches_only_its_own_ports() {
        let policy = Policy::new(Network::None, None, [80]);
        assert_eq!(policy.connect(PUBLIC), None);
        assert_eq!(policy.connect(at(Ipv4Addr::LOCALHOST, 8080)), None);
        let here = policy.listen(at(Ipv4Addr::UNSPECIFIED, 80)).unwrap();
        assert_eq!(policy.connect(at(Ipv4Addr::LOCALHOST, 80)), Some(here));
    }

    /// `Public` reaches the internet and nothing of this host's or its network's.
    #[test]
    fn public_reaches_only_what_the_internet_routes() {
        let policy = Policy::new(Network::Public, None, [80]);
        assert_eq!(policy.connect(PUBLIC), Some(PUBLIC));
        let v6 = at("2606:4700::1111".parse::<Ipv6Addr>().unwrap(), 443);
        assert_eq!(policy.connect(v6), Some(v6));
        for to in [
            at(Ipv4Addr::LOCALHOST, 8080),
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

        let here = policy.listen(at(Ipv4Addr::UNSPECIFIED, 80)).unwrap();
        assert_eq!(policy.connect(at(Ipv4Addr::LOCALHOST, 80)), Some(here));
    }

    /// Only a published port is listened on here, at one port of this host's loopback that
    /// it keeps -- and that the guest's own loopback then reaches too.
    #[test]
    fn a_published_port_is_listened_on_here_and_nothing_else_is() {
        let policy = policy([80]);
        assert_eq!(policy.listen(at(Ipv4Addr::UNSPECIFIED, 8080)), None);
        assert_eq!(policy.listener(80), None);

        let here = policy.listen(at(Ipv4Addr::UNSPECIFIED, 80)).unwrap();
        assert_eq!(here.ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert_eq!(
            policy.listener(80),
            Some(Listeners {
                v4: Some(here.port()),
                v6: None
            })
        );
        assert_eq!(policy.listen(at(Ipv4Addr::UNSPECIFIED, 80)), Some(here));
        assert_eq!(
            policy.listen(at(Ipv6Addr::UNSPECIFIED, 80)),
            Some(at(Ipv6Addr::LOCALHOST, here.port()))
        );

        assert_eq!(policy.connect(at(Ipv4Addr::LOCALHOST, 80)), Some(here));
    }

    /// Its own port over v4 goes through the relay, which reaches a guest listener in either
    /// family; over v6 it goes straight to the listener.
    #[test]
    fn its_own_port_over_v4_goes_through_the_relay() {
        let relay = at(Ipv4Addr::LOCALHOST, 40000);
        let policy = policy([80]).with_relays([(80, relay)]);
        let here = policy.listen(at(Ipv6Addr::UNSPECIFIED, 80)).unwrap();
        assert_eq!(policy.connect(at(Ipv4Addr::LOCALHOST, 80)), Some(relay));
        assert_eq!(policy.connect(at(Ipv6Addr::LOCALHOST, 80)), Some(here));
    }

    /// Its own published port wins over this host's of the same number, and is refused
    /// until something in it listens there.
    #[test]
    fn its_own_listener_comes_first() {
        let policy = policy([80]);
        assert_eq!(policy.connect(at(Ipv4Addr::LOCALHOST, 80)), None);
        let here = policy.listen(at(Ipv4Addr::UNSPECIFIED, 80)).unwrap();
        assert_eq!(policy.connect(at(Ipv4Addr::LOCALHOST, 80)), Some(here));
    }

    /// A second listen in one family takes a port of its own and leaves the other family's
    /// where it was: x11vnc listens on `::1` twice, and its v4 listener is the one that serves.
    #[test]
    fn a_second_listen_in_one_family_leaves_the_other_alone() {
        let policy = policy([5900]);
        let v4 = policy.listen(at(Ipv4Addr::LOCALHOST, 5900)).unwrap();
        let _held_v4 = TcpListener::bind(v4).unwrap();
        let v6 = policy.listen(at(Ipv6Addr::LOCALHOST, 5900)).unwrap();
        assert_eq!(v6.port(), v4.port());
        let _held_v6 = TcpListener::bind(v6).unwrap();
        let again = policy.listen(at(Ipv6Addr::LOCALHOST, 5900)).unwrap();
        assert_ne!(again.port(), v4.port());
        assert_eq!(
            policy.listener(5900),
            Some(Listeners {
                v4: Some(v4.port()),
                v6: Some(again.port())
            })
        );
    }
}
