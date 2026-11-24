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
//! # What leaves for the internet, counted and said
//!
//! Under [`Public`](Network::Public), each flow to the internet -- a stream's `connect`, or a
//! datagram socket's first destination in a minute -- is reported ([`Outbound`]) to the server
//! that started the machine, which is the only record of which machine opened it: off the
//! host, every machine's flows are the host's. Flows past the [`EgressLimits`] the server set
//! are refused, as are flows to port 25, and the refusals are reported too, at most one a
//! second for each reason with how many there were.
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
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use super::{EgressLimits, NAMESERVER, Network};
use crate::machine::wire::{Outbound, Protocol, Refusal, Refused};

/// The DNS port, the one [`NAMESERVER`] answers on.
const DNS_PORT: u16 = 53;

/// SMTP's port between servers, which a machine on a public network never reaches: mail sent
/// from a shared address is how that address is blocklisted for everyone behind it.
const SMTP_PORT: u16 = 25;

/// How long a destination counts toward [`EgressLimits::destinations_per_minute`], and how long
/// a datagram destination, once reported, is not again.
const MINUTE: Duration = Duration::from_secs(60);

/// How often refusals for one reason are reported at most.
const REFUSALS_EVERY: Duration = Duration::from_secs(1);

/// Where a machine's [`Outbound`] flows are reported.
pub(crate) type Report = Arc<dyn Fn(Outbound) + Send + Sync>;

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

    /// What flows to the internet it may open; none is no limit.
    limits: Option<EgressLimits>,

    /// Where its flows to the internet are reported; none reports nothing.
    report: Option<Report>,

    /// What it has opened and been refused lately, as the limits count it.
    meter: Mutex<Meter>,
}

/// A machine's recent flows to the internet, as its [`EgressLimits`] count them.
struct Meter {
    /// The connections it may open now: one more every `1 / connects_per_second` seconds, up
    /// to a second's worth.
    tokens: f64,
    filled: Option<Instant>,
    /// Each address it reached in the last minute, by when it last did.
    destinations: HashMap<IpAddr, Instant>,
    /// Each datagram destination reported in the last minute, by when it was.
    datagrams: HashMap<SocketAddr, Instant>,
    /// When what is a minute old was last let go of.
    pruned: Option<Instant>,
    /// Each reason flows were refused for: how many since it was last reported, and when that
    /// was.
    refused: HashMap<Refusal, (u32, Option<Instant>)>,
}

impl Meter {
    fn new() -> Self {
        Meter {
            tokens: 0.0,
            filled: None,
            destinations: HashMap::new(),
            datagrams: HashMap::new(),
            pruned: None,
            refused: HashMap::new(),
        }
    }

    /// Whatever is a minute old let go of, at most once a second.
    fn prune(&mut self, now: Instant) {
        if self.pruned.is_some_and(|at| now - at < REFUSALS_EVERY) {
            return;
        }
        self.pruned = Some(now);
        self.destinations.retain(|_, at| now - *at < MINUTE);
        self.datagrams.retain(|_, at| now - *at < MINUTE);
    }

    /// Why a new flow to `to` is refused under `limits`, or none, the flow then counted.
    fn admit(&mut self, to: IpAddr, limits: EgressLimits, now: Instant) -> Option<Refusal> {
        let rate = f64::from(limits.connects_per_second);
        let since = self.filled.map_or(1.0, |at| (now - at).as_secs_f64());
        self.tokens = (self.tokens + since * rate).min(rate);
        self.filled = Some(now);
        if self.tokens < 1.0 {
            return Some(Refusal::Rate);
        }
        if !self.destinations.contains_key(&to)
            && self.destinations.len() >= limits.destinations_per_minute as usize
        {
            return Some(Refusal::Destinations);
        }
        self.tokens -= 1.0;
        None
    }

    /// One more refused for `why`: the count to report now, which starts the count again, or
    /// none while the last report of it is under a second old.
    fn refused(&mut self, why: Refusal, now: Instant) -> Option<u32> {
        let (count, at) = self.refused.entry(why).or_default();
        *count += 1;
        if at.is_some_and(|at| now - at < REFUSALS_EVERY) {
            return None;
        }
        *at = Some(now);
        Some(std::mem::take(count))
    }
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
            limits: None,
            report: None,
            meter: Mutex::new(Meter::new()),
        }
    }

    /// Its flows to the internet held to `limits`, where there are any, and reported to
    /// `report`: what [`Public`](Network::Public) does with them, and nothing else does.
    pub(crate) fn with_egress(mut self, limits: Option<EgressLimits>, report: Report) -> Self {
        self.limits = limits;
        self.report = Some(report);
        self
    }

    /// Where a flow to `to`, which the internet routes, goes at `now`: there, or refused for
    /// port 25 or past its limits. Reported either way, as the module says.
    fn outward(&self, to: SocketAddr, protocol: Protocol, now: Instant) -> Option<SocketAddr> {
        let ip = to.ip().to_canonical();
        let (answer, refused) = {
            let mut meter = self.meter.lock().unwrap();
            meter.prune(now);
            let why = match self.limits {
                _ if to.port() == SMTP_PORT => Some(Refusal::Port),
                // A datagram destination reported in the last minute is no new flow.
                _ if protocol == Protocol::Udp && meter.datagrams.contains_key(&to) => {
                    meter.destinations.insert(ip, now);
                    return Some(to);
                }
                Some(limits) => meter.admit(ip, limits, now),
                None => None,
            };
            match why {
                Some(why) => (None, meter.refused(why, now).map(|count| (why, count))),
                None => {
                    if self.limits.is_some() {
                        meter.destinations.insert(ip, now);
                    }
                    if protocol == Protocol::Udp {
                        meter.datagrams.insert(to, now);
                    }
                    (Some(to), None)
                }
            }
        };
        if let Some(report) = &self.report
            && (answer.is_some() || refused.is_some())
        {
            report(Outbound {
                at: SystemTime::now(),
                protocol,
                to,
                refused: refused.map(|(why, count)| Refused { why, count }),
            });
        }
        answer
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

    /// Where a guest's request for `to` goes, a stream's or a datagram socket's: see the
    /// module's table.
    fn to(&self, to: SocketAddr, protocol: Protocol) -> Option<SocketAddr> {
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
            Network::Public if routed(ip) => self.outward(to, protocol, Instant::now()),
            Network::Public => None,
        }
    }
}

impl krun::TsiPolicy for Policy {
    fn connect(&self, to: SocketAddr) -> Option<SocketAddr> {
        self.to(to, Protocol::Tcp)
    }

    fn datagram(&self, to: SocketAddr) -> Option<SocketAddr> {
        self.to(to, Protocol::Udp)
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

    /// A policy under `Public`, held to `limits`, and what it reports.
    fn reporting(limits: Option<EgressLimits>) -> (Policy, Arc<Mutex<Vec<Outbound>>>) {
        let reports = Arc::new(Mutex::new(Vec::new()));
        let kept = Arc::clone(&reports);
        let policy = Policy::new(Network::Public, None, [80]).with_egress(
            limits,
            Arc::new(move |outbound| kept.lock().unwrap().push(outbound)),
        );
        (policy, reports)
    }

    /// A report: where to, how, and why refused if it was.
    type Reported = (SocketAddr, Protocol, Option<(Refusal, u32)>);

    /// What was reported since this was last asked.
    fn taken(reports: &Mutex<Vec<Outbound>>) -> Vec<Reported> {
        let reports = std::mem::take(&mut *reports.lock().unwrap());
        (reports.into_iter())
            .map(|o| (o.to, o.protocol, o.refused.map(|r| (r.why, r.count))))
            .collect()
    }

    /// Each stream to the internet is reported, and each datagram destination once a minute;
    /// what is not the internet's -- loopback, the resolver, private addresses -- is not.
    #[test]
    fn each_flow_to_the_internet_is_reported_and_nothing_else_is() {
        let (policy, reports) = reporting(None);
        let other = at(Ipv4Addr::new(1, 1, 1, 1), 53);
        assert_eq!(policy.connect(PUBLIC), Some(PUBLIC));
        assert_eq!(policy.connect(PUBLIC), Some(PUBLIC));
        assert_eq!(policy.datagram(other), Some(other));
        assert_eq!(policy.datagram(other), Some(other));
        policy.connect(at(Ipv4Addr::LOCALHOST, 8080));
        policy.connect(at(Ipv4Addr::new(10, 0, 0, 1), 80));
        policy.datagram(at(NAMESERVER, 53));
        assert_eq!(
            taken(&reports),
            [
                (PUBLIC, Protocol::Tcp, None),
                (PUBLIC, Protocol::Tcp, None),
                (other, Protocol::Udp, None),
            ]
        );
        // A minute on, a datagram destination is a new flow again.
        let later = Instant::now() + MINUTE + REFUSALS_EVERY;
        assert_eq!(policy.outward(other, Protocol::Udp, later), Some(other));
        assert_eq!(taken(&reports), [(other, Protocol::Udp, None)]);
    }

    /// Nothing reaches port 25 on the internet, and the refusals are reported, at most one a
    /// second with how many there were.
    #[test]
    fn port_25_is_refused_and_its_refusals_counted() {
        let (policy, reports) = reporting(None);
        let smtp = at(Ipv4Addr::new(142, 250, 0, 26), 25);
        let now = Instant::now();
        for _ in 0..3 {
            assert_eq!(policy.outward(smtp, Protocol::Tcp, now), None);
        }
        assert_eq!(
            policy.outward(smtp, Protocol::Tcp, now + REFUSALS_EVERY),
            None
        );
        assert_eq!(
            taken(&reports),
            [
                (smtp, Protocol::Tcp, Some((Refusal::Port, 1))),
                (smtp, Protocol::Tcp, Some((Refusal::Port, 3))),
            ]
        );
        // Off the internet, port 25 is as any other port is under a network.
        let host = Policy::new(Network::Host, None, []);
        assert_eq!(host.connect(smtp), Some(smtp));
    }

    /// Past its connections a second, a new flow is refused until the second's worth comes
    /// back; a datagram destination already reported is no new flow.
    #[test]
    fn past_its_rate_a_new_flow_is_refused() {
        let limits = EgressLimits {
            connects_per_second: 2,
            destinations_per_minute: 100,
        };
        let (policy, reports) = reporting(Some(limits));
        let dns = at(Ipv4Addr::new(1, 1, 1, 1), 53);
        let now = Instant::now();
        assert_eq!(policy.outward(PUBLIC, Protocol::Tcp, now), Some(PUBLIC));
        assert_eq!(policy.outward(dns, Protocol::Udp, now), Some(dns));
        assert_eq!(policy.outward(PUBLIC, Protocol::Tcp, now), None);
        assert_eq!(policy.outward(PUBLIC, Protocol::Tcp, now), None);
        assert_eq!(policy.outward(dns, Protocol::Udp, now), Some(dns));
        let second = now + Duration::from_secs(1);
        assert_eq!(policy.outward(PUBLIC, Protocol::Tcp, second), Some(PUBLIC));
        assert_eq!(policy.outward(PUBLIC, Protocol::Tcp, second), Some(PUBLIC));
        assert_eq!(policy.outward(PUBLIC, Protocol::Tcp, second), None);
        assert_eq!(
            taken(&reports),
            [
                (PUBLIC, Protocol::Tcp, None),
                (dns, Protocol::Udp, None),
                (PUBLIC, Protocol::Tcp, Some((Refusal::Rate, 1))),
                (PUBLIC, Protocol::Tcp, None),
                (PUBLIC, Protocol::Tcp, None),
                (PUBLIC, Protocol::Tcp, Some((Refusal::Rate, 2))),
            ]
        );
    }

    /// Past its distinct addresses a minute, a new address is refused and one reached already
    /// is reached again; a minute on, the new one is reached.
    #[test]
    fn past_its_destinations_a_new_address_is_refused() {
        let limits = EgressLimits {
            connects_per_second: 100,
            destinations_per_minute: 2,
        };
        let (policy, reports) = reporting(Some(limits));
        let [a, b, c] = [1, 2, 3].map(|last| at(Ipv4Addr::new(93, 184, 215, last), 443));
        let now = Instant::now();
        assert_eq!(policy.outward(a, Protocol::Tcp, now), Some(a));
        assert_eq!(policy.outward(b, Protocol::Tcp, now), Some(b));
        assert_eq!(policy.outward(c, Protocol::Tcp, now), None);
        let other_port = at(a.ip(), 80);
        assert_eq!(
            policy.outward(other_port, Protocol::Tcp, now),
            Some(other_port)
        );
        let later = now + MINUTE + REFUSALS_EVERY;
        assert_eq!(policy.outward(c, Protocol::Tcp, later), Some(c));
        assert_eq!(
            taken(&reports),
            [
                (a, Protocol::Tcp, None),
                (b, Protocol::Tcp, None),
                (c, Protocol::Tcp, Some((Refusal::Destinations, 1))),
                (other_port, Protocol::Tcp, None),
                (c, Protocol::Tcp, None),
            ]
        );
    }
}
