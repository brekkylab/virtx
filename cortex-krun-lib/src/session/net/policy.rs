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
//!   the guest asks for          it gets
//!   ───────────────────────────────────────────────────────────────────────────
//!   NAMESERVER:53               this host's resolver
//!   loopback, or 0.0.0.0        its own published listener on that port, or refused
//!   anywhere else               where it asked
//!   listen on a published port  a listener on this host's loopback
//!   listen on any other port    a listener in the guest, and nothing here
//! ```
//!
//! **This host's loopback is not the guest's to reach.** What listens there is the
//! operator's -- an agent, a database, a model server -- and a session given a network is
//! not thereby given all of them. A guest only falls through to here for loopback when its
//! own stack had nothing listening on that port, so a refusal is exactly what it would have
//! been told without a network.
//!
//! **Everything else is.** On is what this host reaches, its LAN included. A session that
//! has to be kept off part of a network is kept off it by the host, which is where every
//! other process on this machine is kept off it too.
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

use super::NAMESERVER;

/// The DNS port, the one [`NAMESERVER`] answers on.
const DNS_PORT: u16 = 53;

/// Everything a guest's request is judged by: where names go, and which console ports are
/// published.
pub(crate) struct Policy {
    /// Where a query to [`NAMESERVER`] goes, and `None` for a host that names no resolver --
    /// whose guest then resolves no names, which is what every other process here gets.
    resolver: Option<Ipv4Addr>,

    /// The console ports a connection from this host may reach.
    published: HashSet<u16>,

    /// Where on this host's loopback each published console port is listened on, once the
    /// guest has listened on it.
    listening: Mutex<HashMap<u16, u16>>,
}

impl Policy {
    pub(crate) fn new(
        resolver: Option<Ipv4Addr>,
        published: impl IntoIterator<Item = u16>,
    ) -> Self {
        Policy {
            resolver,
            published: published.into_iter().collect(),
            listening: Mutex::new(HashMap::new()),
        }
    }

    /// Where on this host a connection reaches the guest's listener on `console`, or `None`
    /// while nothing in the guest listens there for this host to reach.
    pub(crate) fn listener(&self, console: u16) -> Option<u16> {
        self.listening.lock().unwrap().get(&console).copied()
    }

    /// The loopback port `console` is listened on in `family`'s family: the previous one while
    /// nothing holds it, a fresh one otherwise. `None` is a host with no free port.
    fn listen_port(&self, console: u16, family: IpAddr) -> Option<u16> {
        let mut listening = self.listening.lock().unwrap();
        let here = loopback(family);
        // A held port is either our guest's live listener or someone else's; we can't
        // tell which, so it isn't handed out again.
        let kept = listening
            .get(&console)
            .copied()
            .filter(|&port| TcpListener::bind((here, port)).is_ok());
        let port = match kept {
            Some(port) => port,
            None => TcpListener::bind((here, 0))
                .and_then(|probe| probe.local_addr())
                .ok()?
                .port(),
        };
        listening.insert(console, port);
        Some(port)
    }
}

impl krun::TsiPolicy for Policy {
    fn connect(&self, to: SocketAddr) -> Option<SocketAddr> {
        let ip = to.ip().to_canonical();
        if ip == IpAddr::V4(NAMESERVER) {
            return (to.port() == DNS_PORT)
                .then_some(self.resolver)
                .flatten()
                .map(|resolver| SocketAddr::new(resolver.into(), DNS_PORT));
        }
        // What `0.0.0.0` means as a destination on Linux is this machine, so it is loopback
        // by another name.
        if ip.is_loopback() || ip.is_unspecified() {
            return self
                .listener(to.port())
                .map(|port| SocketAddr::new(loopback(ip), port));
        }
        Some(to)
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

    /// On is what this host reaches: anywhere but its loopback goes where it was asked.
    #[test]
    fn the_outside_is_where_it_was_asked_for() {
        let policy = Policy::new(None, []);
        assert_eq!(policy.connect(PUBLIC), Some(PUBLIC));
        let lan = at(Ipv4Addr::new(192, 168, 1, 1), 80);
        assert_eq!(policy.connect(lan), Some(lan));
    }

    /// This host's loopback is the operator's, whatever the port and however it is spelled.
    #[test]
    fn this_hosts_loopback_is_refused() {
        let policy = Policy::new(None, []);
        for to in [
            at(Ipv4Addr::LOCALHOST, 8080),
            at(Ipv4Addr::new(127, 1, 2, 3), 22),
            at(Ipv4Addr::UNSPECIFIED, 8080),
            at(Ipv6Addr::LOCALHOST, 8080),
            at(Ipv4Addr::LOCALHOST.to_ipv6_mapped(), 8080),
        ] {
            assert_eq!(policy.connect(to), None, "{to}");
        }
    }

    /// The guest's resolver is this host's, for DNS and nothing else.
    #[test]
    fn the_nameserver_is_this_hosts_resolver() {
        let resolver = Ipv4Addr::new(192, 168, 1, 1);
        let policy = Policy::new(Some(resolver), []);
        assert_eq!(policy.connect(at(NAMESERVER, 53)), Some(at(resolver, 53)));
        assert_eq!(policy.connect(at(NAMESERVER, 80)), None);
        assert_eq!(Policy::new(None, []).connect(at(NAMESERVER, 53)), None);
    }

    /// Only a published port is listened on here, at one port of this host's loopback that
    /// it keeps -- and that the guest's own loopback then reaches too.
    #[test]
    fn a_published_port_is_listened_on_here_and_nothing_else_is() {
        let policy = Policy::new(None, [80]);
        assert_eq!(policy.listen(at(Ipv4Addr::UNSPECIFIED, 8080)), None);
        assert_eq!(policy.listener(80), None);

        let here = policy.listen(at(Ipv4Addr::UNSPECIFIED, 80)).unwrap();
        assert_eq!(here.ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert_eq!(policy.listener(80), Some(here.port()));
        assert_eq!(policy.listen(at(Ipv4Addr::UNSPECIFIED, 80)), Some(here));
        assert_eq!(
            policy.listen(at(Ipv6Addr::UNSPECIFIED, 80)),
            Some(at(Ipv6Addr::LOCALHOST, here.port()))
        );

        assert_eq!(policy.connect(at(Ipv4Addr::LOCALHOST, 80)), Some(here));
    }
}
