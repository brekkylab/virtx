use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener};
use std::ops::Range;
use std::time::Duration;

use polling::{Event, Events, Poller};
use smoltcp::iface::{Config, Interface, PollResult, Route, SocketSet};
use smoltcp::time::Instant;
use smoltcp::wire::{
    EthernetAddress, EthernetFrame, EthernetProtocol, IpCidr, IpProtocol, Ipv4Packet, Ipv6Packet,
    TcpPacket, UdpPacket,
};

use super::LAN;
use super::dns::Dns;
use super::egress::{Egress, is_dns};
use super::flow::{EndpointPair, Flows};
use super::link::Link;
use crate::machine::ClientRelay;

/// The link's key in the poller, below the one the poller keeps for itself. The nameserver's
/// socket is under it, and a published port's listener under that, by its place; a flow's is
/// its own key in [`Flows`].
const LINK: usize = usize::MAX - 1;
const DNS: usize = LINK - 1;

pub(in crate::machine::net) struct Gateway {
    // First, so it is dropped first: every source it waits on is still open when it goes.
    poller: Poller,
    link: Link,
    iface: Interface,
    sockets: SocketSet<'static>,
    egress: Egress,
    flows: Flows,
    dns: Dns,
    published: Vec<(u16, TcpListener)>,
    started: std::time::Instant,
}

impl Gateway {
    /// With the embedder's [`Outward`](crate::outward::Outward), where it set one, asked of
    /// each flow to the internet with `given`.
    pub(in crate::machine::net) fn new(
        link: Link,
        egress: bool,
        given: serde_json::Value,
        published: Vec<(u16, TcpListener)>,
        client: Option<ClientRelay>,
    ) -> io::Result<Self> {
        let mut decided = Egress::new(egress, client);
        if let Some(outward) = crate::outward::outward() {
            decided = decided.with_outward(outward, given);
        }
        Self::with_egress(link, decided, published)
    }

    fn with_egress(
        mut link: Link,
        egress: Egress,
        published: Vec<(u16, TcpListener)>,
    ) -> io::Result<Gateway> {
        let mut config = Config::new(EthernetAddress(LAN.gateway_mac).into());
        config.random_seed =
            std::hash::BuildHasher::hash_one(&std::hash::RandomState::new(), std::process::id());
        let mut iface = Interface::new(config, &mut link, Instant::ZERO);
        iface.update_ip_addrs(|addrs| {
            let _ = addrs.push(IpCidr::new(LAN.gateway.into(), LAN.prefix));
            let _ = addrs.push(IpCidr::new(LAN.gateway_v6.into(), LAN.prefix_v6));
            // On the guest's link, so the guest asks for it by neighbor discovery, which smoltcp
            // answers only for an address of its own.
            let _ = addrs.push(IpCidr::new(LAN.nameserver_v6.into(), LAN.prefix_v6));
        });
        // Every address the guest sends to is this end's to answer for, as the policy says.
        iface.set_any_ip(true);
        // A published port is reached at the guest's loopback, where a program listening on
        // `127.0.0.1` alone is reached too: the guest takes it on its link, as the agent says.
        iface.routes_mut().update(|routes| {
            let _ = routes.push(Route {
                cidr: IpCidr::new(Ipv4Addr::new(127, 0, 0, 0).into(), 8),
                via_router: LAN.guest.into(),
                preferred_until: None,
                expires_at: None,
            });
        });

        let poller = Poller::new()?;
        // SAFETY: the poller is dropped before the link and the listeners.
        unsafe { poller.add(link.socket(), Event::none(LINK))? };
        for (i, (_, listener)) in published.iter().enumerate() {
            listener.set_nonblocking(true)?;
            unsafe { poller.add(listener, Event::readable(DNS - 1 - i))? };
        }
        let dns = Dns::new(&poller, DNS, egress.resolver.filter(|_| egress.enabled));
        Ok(Gateway {
            poller,
            link,
            iface,
            sockets: SocketSet::new(Vec::new()),
            egress,
            flows: Flows::default(),
            dns,
            published,
            started: std::time::Instant::now(),
        })
    }

    /// Carry the guest's network until the device closes its end of the link.
    pub(in crate::machine::net) fn run(mut self) -> io::Result<()> {
        let mut events = Events::new();
        let mut link_armed = None;
        loop {
            let unsent = self.link.unsent();
            if link_armed != Some(unsent) {
                self.poller
                    .modify(self.link.socket(), Event::new(LINK, true, unsent))?;
                link_armed = Some(unsent);
            }
            let mut timeout = self
                .iface
                .poll_delay(self.now(), &self.sockets)
                .map(|delay| Duration::from_micros(delay.total_micros()));
            if let Some(idle) = self.flows.timeout() {
                timeout = Some(timeout.map_or(idle, |timeout| timeout.min(idle)));
            }
            events.clear();
            self.poller.wait(&mut events, timeout)?;

            let mut link_ready = false;
            for event in events.iter() {
                match event.key {
                    LINK => {
                        link_ready = true;
                        link_armed = None;
                    }
                    DNS => self.dns.ready(),
                    key if key >= DNS - self.published.len() => self.accept(DNS - 1 - key)?,
                    key => self.flows.ready(key),
                }
            }

            let now = self.now();
            if link_ready && !self.link.fill()? {
                return Ok(());
            }
            self.ingress(now);
            self.flows
                .pump(&self.poller, &mut self.sockets, &mut self.link);
            self.dns.pump(&self.poller, &mut self.link);
            self.iface.poll_maintenance(now);
            while self
                .iface
                .poll_egress(now, &mut self.link, &mut self.sockets)
                != PollResult::None
            {}
            self.link.flush()?;
            self.flows.reap(&self.poller, &mut self.sockets);
            self.dns.reap();
        }
    }

    fn now(&self) -> Instant {
        Instant::from_micros(self.started.elapsed().as_micros() as i64)
    }

    /// Take every frame read in: open the flow a frame opens, carry UDP, give smoltcp the rest.
    fn ingress(&mut self, now: Instant) {
        while let Some(frame) = self.link.peek() {
            match Frame::of(frame) {
                Frame::Udp { ends, payload } if is_dns(ends.1) => {
                    self.dns.query(ends, &frame[payload]);
                    self.link.skip();
                }
                Frame::Udp { ends, payload } => {
                    self.flows.udp(&self.poller, ends, &frame[payload], || {
                        self.egress.route(ends.1)
                    });
                    self.link.skip();
                }
                Frame::Syn { ends } => {
                    self.flows.syn(&self.poller, &mut self.sockets, ends, || {
                        self.egress.connect(ends.1)
                    });
                    self.iface
                        .poll_ingress_single(now, &mut self.link, &mut self.sockets);
                }
                Frame::Smoltcp => {
                    self.iface
                        .poll_ingress_single(now, &mut self.link, &mut self.sockets);
                }
                Frame::Drop => self.link.skip(),
            }
        }
    }

    /// Accept what the `i`th published port's listener has, each connection carried to its
    /// port at the guest's loopback, from the gateway.
    fn accept(&mut self, i: usize) -> io::Result<()> {
        let port = self.published[i].0;
        let guest = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        loop {
            let host = match self.published[i].1.accept() {
                Ok((host, _)) => host,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            };
            self.flows.accept(
                &self.poller,
                &mut self.sockets,
                self.iface.context(),
                host.into(),
                guest,
            );
        }
        self.poller
            .modify(&self.published[i].1, Event::readable(DNS - 1 - i))
    }
}

/// What a frame from the guest is, as far as the stack takes it apart.
enum Frame {
    /// A TCP connection's first segment.
    Syn { ends: EndpointPair },
    /// A datagram, its payload where it lies in the frame.
    Udp {
        ends: EndpointPair,
        payload: Range<usize>,
    },
    /// smoltcp's: ARP, the rest of TCP, ICMP to this end, and neighbor discovery.
    Smoltcp,
    /// Nothing to answer. ICMP to anywhere but this end is dropped rather than left to smoltcp,
    /// which answers for every address and would echo a ping to one the guest cannot reach. So
    /// is a v6 packet with extension headers, which the guest's TCP and UDP do not send.
    Drop,
}

impl Frame {
    fn of(frame: &[u8]) -> Frame {
        let Ok(ethernet) = EthernetFrame::new_checked(frame) else {
            return Frame::Drop;
        };
        let (src, dst, protocol, payload, header_len, fragment) = match ethernet.ethertype() {
            EthernetProtocol::Ipv4 => match Ipv4Packet::new_checked(ethernet.payload()) {
                Ok(ip) => (
                    IpAddr::from(ip.src_addr()),
                    IpAddr::from(ip.dst_addr()),
                    ip.next_header(),
                    ip.payload(),
                    ip.header_len() as usize,
                    ip.more_frags() || ip.frag_offset() != 0,
                ),
                Err(_) => return Frame::Drop,
            },
            EthernetProtocol::Ipv6 => match Ipv6Packet::new_checked(ethernet.payload()) {
                Ok(ip) => (
                    IpAddr::from(ip.src_addr()),
                    IpAddr::from(ip.dst_addr()),
                    ip.next_header(),
                    ip.payload(),
                    ip.header_len(),
                    false,
                ),
                Err(_) => return Frame::Drop,
            },
            _ => return Frame::Smoltcp,
        };
        let this_end = [
            LAN.gateway.into(),
            LAN.nameserver.into(),
            LAN.gateway_v6.into(),
            LAN.nameserver_v6.into(),
        ]
        .contains(&dst);
        match protocol {
            IpProtocol::Tcp if !fragment => match TcpPacket::new_checked(payload) {
                Ok(tcp) if tcp.syn() && !tcp.ack() => Frame::Syn {
                    ends: (
                        SocketAddr::new(src, tcp.src_port()),
                        SocketAddr::new(dst, tcp.dst_port()),
                    ),
                },
                Ok(_) => Frame::Smoltcp,
                Err(_) => Frame::Drop,
            },
            IpProtocol::Udp if !fragment => match UdpPacket::new_checked(payload) {
                Ok(udp) => {
                    let at = EthernetFrame::<&[u8]>::header_len() + header_len;
                    Frame::Udp {
                        ends: (
                            SocketAddr::new(src, udp.src_port()),
                            SocketAddr::new(dst, udp.dst_port()),
                        ),
                        payload: at + 8..at + udp.len() as usize,
                    }
                }
                Err(_) => Frame::Drop,
            },
            IpProtocol::Icmp if this_end => Frame::Smoltcp,
            // Neighbor discovery goes to multicast groups as well as to this end.
            IpProtocol::Icmpv6 if this_end || dst.is_multicast() => Frame::Smoltcp,
            _ => Frame::Drop,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read as _, Write as _};
    use std::net::{Shutdown, SocketAddr, TcpStream, UdpSocket};
    use std::thread;
    use std::time::Instant as Clock;

    use smoltcp::iface::SocketHandle;
    use smoltcp::socket::{Socket, tcp, udp};
    use smoltcp::wire::IpAddress;

    use super::super::link::{self, DeviceEnd};
    use super::*;

    /// A guest at the far end of a stack's link: smoltcp of its own, at the guest's address and
    /// its loopback, routed through the gateway.
    struct Guest {
        link: Link,
        iface: Interface,
        sockets: SocketSet<'static>,
        started: Clock,
    }

    /// The link opened as the device's is: the device's end, and the gateway's to connect.
    fn opened() -> (DeviceEnd, link::Pending) {
        #[cfg(unix)]
        let opened = link::open();
        #[cfg(windows)]
        let opened = {
            use std::sync::atomic::{AtomicU32, Ordering};
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let name = format!("virtx-net-test-{}-{n}.sock", std::process::id());
            link::open(std::env::temp_dir().join(name))
        };
        opened.unwrap()
    }

    impl Guest {
        /// The guest at the device's end of a link, its gateway run on a thread of its own.
        fn new(egress: Egress, published: Vec<(u16, TcpListener)>) -> Guest {
            let (end, pending) = opened();
            thread::spawn(move || {
                Gateway::with_egress(pending.connect().unwrap(), egress, published)
                    .unwrap()
                    .run()
            });
            Guest::at(end)
        }

        /// The guest at the device's end of a link, `end`.
        fn at(end: DeviceEnd) -> Guest {
            let theirs = match end {
                #[cfg(unix)]
                DeviceEnd::Fd(fd) => fd.into(),
                #[cfg(windows)]
                DeviceEnd::Path(path) => {
                    let socket =
                        socket2::Socket::new(socket2::Domain::UNIX, socket2::Type::STREAM, None)
                            .unwrap();
                    socket
                        .connect(&socket2::SockAddr::unix(path).unwrap())
                        .unwrap();
                    socket
                }
            };
            let mut link = Link::new(theirs).unwrap();
            let config = Config::new(EthernetAddress(LAN.guest_mac).into());
            let mut iface = Interface::new(config, &mut link, Instant::ZERO);
            iface.update_ip_addrs(|addrs| {
                addrs
                    .push(IpCidr::new(LAN.guest.into(), LAN.prefix))
                    .unwrap();
                addrs
                    .push(IpCidr::new(Ipv4Addr::LOCALHOST.into(), 8))
                    .unwrap();
                addrs
                    .push(IpCidr::new(LAN.guest_v6.into(), LAN.prefix_v6))
                    .unwrap();
            });
            let routes = iface.routes_mut();
            routes.add_default_ipv4_route(LAN.gateway).unwrap();
            routes.add_default_ipv6_route(LAN.gateway_v6).unwrap();
            Guest {
                link,
                iface,
                sockets: SocketSet::new(Vec::new()),
                started: Clock::now(),
            }
        }

        /// Turn the guest's stack until `done` says so, failing after ten seconds.
        fn until(&mut self, what: &str, mut done: impl FnMut(&mut SocketSet<'static>) -> bool) {
            let deadline = Clock::now() + Duration::from_secs(10);
            while !done(&mut self.sockets) {
                assert!(Clock::now() < deadline, "timed out waiting until {what}");
                self.turn();
                thread::sleep(Duration::from_millis(1));
            }
        }

        /// Take what the link has, and send what the stack then has to.
        fn turn(&mut self) {
            let now = Instant::from_micros(self.started.elapsed().as_micros() as i64);
            self.link.fill().unwrap();
            self.iface.poll(now, &mut self.link, &mut self.sockets);
            self.link.flush().unwrap();
        }

        fn connect(&mut self, to: SocketAddr) -> SocketHandle {
            let buffer = || tcp::SocketBuffer::new(vec![0; 256 << 10]);
            let mut socket = tcp::Socket::new(buffer(), buffer());
            let to = (IpAddress::from(to.ip()), to.port());
            socket.connect(self.iface.context(), to, 40000).unwrap();
            self.sockets.add(socket)
        }
    }

    /// A network whose resolver is `at`, so what the guest sends to its nameserver reaches a
    /// socket of the test's.
    fn resolved_at(at: SocketAddr) -> Egress {
        Egress {
            enabled: true,
            resolver: Some(at),
            client: None,
            outward: None,
        }
    }

    const DNS: SocketAddr = SocketAddr::new(IpAddr::V4(LAN.nameserver), 53);
    const DNS_V6: SocketAddr = SocketAddr::new(IpAddr::V6(LAN.nameserver_v6), 53);

    /// A connection carries a megabyte each way, and each side's close reaches the other, on v4
    /// and v6.
    #[test]
    fn a_connection_carries_both_ways_and_ends_from_each_side() {
        for dns in [DNS, DNS_V6] {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            let mut guest = Guest::new(resolved_at(listener.local_addr().unwrap()), vec![]);
            let handle = guest.connect(dns);
            let sent: Vec<u8> = (0..1 << 20).map(|i| (i % 251) as u8).collect();

            let host = thread::spawn({
                let sent = sent.clone();
                move || {
                    let (mut host, _) = listener.accept().unwrap();
                    let mut got = Vec::new();
                    host.read_to_end(&mut got).unwrap();
                    assert!(
                        got == sent,
                        "the host got {} bytes, not what was sent",
                        got.len()
                    );
                    host.write_all(&sent).unwrap();
                }
            });
            let mut at = 0;
            guest.until("the guest has sent it all", |sockets| {
                let socket = sockets.get_mut::<tcp::Socket>(handle);
                if socket.can_send() {
                    at += socket.send_slice(&sent[at..]).unwrap();
                    if at == sent.len() {
                        socket.close();
                    }
                }
                at == sent.len()
            });
            let mut got = Vec::new();
            guest.until("the host's end has closed", |sockets| {
                let socket = sockets.get_mut::<tcp::Socket>(handle);
                while socket.can_recv() {
                    socket
                        .recv(|buf| (buf.len(), got.extend_from_slice(buf)))
                        .unwrap();
                }
                !socket.may_recv() && got.len() == sent.len()
            });
            assert!(
                got == sent,
                "the guest got {} bytes, not what was sent",
                got.len()
            );
            host.join().unwrap();
        }
    }

    /// What the guest sends after the host has closed its side reaches the host whole, though
    /// the guest's close is taken while the gateway still holds what the host has not: the
    /// connection outlasts the close until it has.
    #[test]
    fn what_the_guest_sends_after_the_host_closes_reaches_the_host_whole() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let mut guest = Guest::new(resolved_at(listener.local_addr().unwrap()), vec![]);
        let handle = guest.connect(DNS);
        let (told, told_host) = std::sync::mpsc::channel();

        let host = thread::spawn(move || {
            let (mut host, _) = listener.accept().unwrap();
            host.shutdown(Shutdown::Write).unwrap();
            // A little at a time until the guest has closed, so its close arrives while the
            // gateway still holds what the host has not taken.
            let mut got = 0;
            let mut step = vec![0; 8 << 10];
            told_host.recv().unwrap();
            while told_host.try_recv().is_err() {
                host.read_exact(&mut step).unwrap();
                got += step.len();
                thread::sleep(Duration::from_millis(50));
            }
            let mut rest = Vec::new();
            host.read_to_end(&mut rest).unwrap();
            got + rest.len()
        });
        // As much as the way to the host holds, and the guest's own buffer full behind it.
        let (chunk, mut sent, mut moved) = (vec![7; 64 << 10], 0, Clock::now());
        guest.until("the way to the host is full", |sockets| {
            let socket = sockets.get_mut::<tcp::Socket>(handle);
            while socket.can_send() {
                sent += socket.send_slice(&chunk).unwrap();
                moved = Clock::now();
            }
            moved.elapsed() > Duration::from_millis(200)
        });
        guest.sockets.get_mut::<tcp::Socket>(handle).close();
        told.send(()).unwrap();
        guest.until("the guest's side has closed", |sockets| {
            sockets.get::<tcp::Socket>(handle).state() == tcp::State::Closed
        });
        told.send(()).unwrap();
        assert_eq!(host.join().unwrap(), sent);
    }

    /// A connection the policy sends nowhere -- this host, at the gateway -- is reset.
    #[test]
    fn a_connection_the_policy_refuses_is_reset() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut guest = Guest::new(resolved_at(listener.local_addr().unwrap()), vec![]);
        let handle = guest.connect(SocketAddr::from((LAN.gateway, port)));
        guest.until("the connection is reset", |sockets| {
            sockets.get::<tcp::Socket>(handle).state() == tcp::State::Closed
        });
    }

    /// A connection the host refuses is reset without being answered first, so the guest's
    /// `connect` fails as one from this host does.
    #[test]
    fn a_connection_the_host_refuses_is_reset_without_being_answered() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let refused = listener.local_addr().unwrap();
        drop(listener);
        let mut guest = Guest::new(resolved_at(refused), vec![]);
        let handle = guest.connect(DNS);
        guest.until("the connection is reset", |sockets| {
            let state = sockets.get::<tcp::Socket>(handle).state();
            assert_ne!(
                state,
                tcp::State::Established,
                "the guest's SYN was answered"
            );
            state == tcp::State::Closed
        });
    }

    /// The guest's SYN is not answered while the host's connect is not.
    #[test]
    fn a_connection_is_not_answered_before_the_hosts_connect_is() {
        // TEST-NET-1, which nothing answers: the host's connect waits, or fails at once.
        let mut guest = Guest::new(resolved_at("192.0.2.1:9".parse().unwrap()), vec![]);
        let handle = guest.connect(DNS);
        let deadline = Clock::now() + Duration::from_millis(500);
        guest.until("half a second has passed", |sockets| {
            let state = sockets.get::<tcp::Socket>(handle).state();
            assert_ne!(
                state,
                tcp::State::Established,
                "the guest's SYN was answered"
            );
            Clock::now() >= deadline
        });
    }

    /// A relay for the client's port 9222 at `relay`, with no network.
    fn relayed(relay: &TcpListener) -> Egress {
        Egress {
            enabled: false,
            resolver: None,
            client: Some(ClientRelay {
                address: relay.local_addr().unwrap(),
                ports: vec![9222],
                key: "k".repeat(64),
            }),
            outward: None,
        }
    }

    /// A connection to a client's port opens with the relay's greeting, and is answered only
    /// once the relay accepts it: then it carries, with no network.
    #[test]
    fn a_clients_port_is_answered_once_the_relay_accepts_it() {
        let relay = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let egress = relayed(&relay);
        let greeting = egress.client.as_ref().unwrap().greeting(9222);
        let mut guest = Guest::new(egress, vec![]);
        let handle = guest.connect(SocketAddr::from((LAN.gateway, 9222)));
        let (accept, accepted) = std::sync::mpsc::channel();

        let host = thread::spawn(move || {
            let (mut host, _) = relay.accept().unwrap();
            let mut said = vec![0; greeting.len()];
            host.read_exact(&mut said).unwrap();
            assert_eq!(said, greeting);
            accepted.recv().unwrap();
            host.write_all(&[crate::machine::CLIENT_ACCEPTED]).unwrap();
            let mut got = [0u8; 2];
            host.read_exact(&mut got).unwrap();
            got
        });
        let deadline = Clock::now() + Duration::from_millis(300);
        guest.until("a while has passed unanswered", |sockets| {
            let state = sockets.get::<tcp::Socket>(handle).state();
            assert_ne!(state, tcp::State::Established, "answered before the relay");
            Clock::now() >= deadline
        });
        accept.send(()).unwrap();
        guest.until("the connection is answered", |sockets| {
            sockets.get::<tcp::Socket>(handle).state() == tcp::State::Established
        });
        guest
            .sockets
            .get_mut::<tcp::Socket>(handle)
            .send_slice(b"hi")
            .unwrap();
        guest.until("the relay has what was sent", |_| host.is_finished());
        assert_eq!(&host.join().unwrap(), b"hi");
    }

    /// A connection to a client's port the relay refuses, by closing it, is reset without being
    /// answered.
    #[test]
    fn a_clients_port_the_relay_refuses_is_reset_without_being_answered() {
        let relay = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let mut guest = Guest::new(relayed(&relay), vec![]);
        let handle = guest.connect(SocketAddr::from((LAN.gateway, 9222)));
        let host = thread::spawn(move || drop(relay.accept().unwrap()));
        guest.until("the connection is reset", |sockets| {
            let state = sockets.get::<tcp::Socket>(handle).state();
            assert_ne!(
                state,
                tcp::State::Established,
                "the guest's SYN was answered"
            );
            state == tcp::State::Closed
        });
        host.join().unwrap();
    }

    /// While the guest's SYN is held for the host's connect, the gateway sleeps until something
    /// arrives rather than turning at once.
    #[test]
    fn a_held_syn_leaves_the_gateway_idle() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let (end, pending) = opened();
        let mut guest = Guest::at(end);
        let egress = resolved_at(listener.local_addr().unwrap());
        let mut gateway = Gateway::with_egress(pending.connect().unwrap(), egress, vec![]).unwrap();
        guest.connect(DNS);
        // Turned by hand, its poller never waited on, the gateway is not told the host's connect
        // was answered, and holds the guest's SYN.
        let deadline = Clock::now() + Duration::from_secs(10);
        let now = loop {
            assert!(Clock::now() < deadline, "the gateway took no SYN");
            guest.turn();
            let now = gateway.now();
            gateway.link.fill().unwrap();
            gateway.ingress(now);
            gateway
                .flows
                .pump(&gateway.poller, &mut gateway.sockets, &mut gateway.link);
            while gateway
                .iface
                .poll_egress(now, &mut gateway.link, &mut gateway.sockets)
                != PollResult::None
            {}
            gateway.link.flush().unwrap();
            let held = gateway.sockets.iter().any(|(_, socket)| {
                matches!(socket, Socket::Tcp(tcp) if tcp.state() == tcp::State::SynReceived)
            });
            if held {
                break now;
            }
            thread::sleep(Duration::from_millis(1));
        };
        assert_ne!(
            gateway.iface.poll_delay(now, &gateway.sockets),
            Some(smoltcp::time::Duration::ZERO),
            "the gateway would turn at once"
        );
    }

    /// A query to the nameserver reaches the resolver, and its answer comes back from the
    /// nameserver with the query's own ID, on v4 and v6.
    #[test]
    fn a_query_is_answered_from_the_nameserver_with_its_own_id() {
        for dns in [DNS, DNS_V6] {
            let host = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            let mut guest = Guest::new(resolved_at(host.local_addr().unwrap()), vec![]);
            let buffer =
                || udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 4], vec![0; 1 << 16]);
            let mut socket = udp::Socket::new(buffer(), buffer());
            socket.bind(40000).unwrap();
            let handle = guest.sockets.add(socket);
            let to = (IpAddress::from(dns.ip()), dns.port());
            guest
                .sockets
                .get_mut::<udp::Socket>(handle)
                .send_slice(b"\x12\x34query", to)
                .unwrap();

            guest.until("the guest's query is out", |sockets| {
                sockets.get::<udp::Socket>(handle).send_queue() == 0
            });
            let mut buf = [0; 16];
            let (n, from) = host.recv_from(&mut buf).unwrap();
            assert_eq!(&buf[2..n], b"query");
            host.send_to(&[&buf[..2], b"answer"].concat(), from)
                .unwrap();
            let mut reply = None;
            guest.until("the answer has come back", |sockets| {
                if let Ok((data, meta)) = sockets.get_mut::<udp::Socket>(handle).recv() {
                    reply = Some((data.to_vec(), meta.endpoint));
                }
                reply.is_some()
            });
            let (data, endpoint) = reply.unwrap();
            assert_eq!(data, b"\x12\x34answer");
            assert_eq!(endpoint, to.into());
        }
    }

    /// Queries from many ports, with the same ID, go to the resolver from one socket, and each
    /// answer comes back to the port its query came from, whatever order they come in.
    #[test]
    fn queries_from_many_ports_share_one_socket_and_each_answer_finds_its_own() {
        let host = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let mut guest = Guest::new(resolved_at(host.local_addr().unwrap()), vec![]);
        let to = (IpAddress::from(DNS.ip()), DNS.port());
        let handles: Vec<_> = (0..8u8)
            .map(|i| {
                let buffer = || {
                    udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 4], vec![0; 1 << 16])
                };
                let mut socket = udp::Socket::new(buffer(), buffer());
                socket.bind(40000 + u16::from(i)).unwrap();
                socket.send_slice(&[0, 1, i], to).unwrap();
                guest.sockets.add(socket)
            })
            .collect();

        guest.until("the guest's queries are out", |sockets| {
            handles
                .iter()
                .all(|&handle| sockets.get::<udp::Socket>(handle).send_queue() == 0)
        });
        let mut got = Vec::new();
        for _ in &handles {
            let mut buf = [0; 16];
            let (n, from) = host.recv_from(&mut buf).unwrap();
            got.push((buf[..n].to_vec(), from));
        }
        assert!(
            got.iter().all(|(_, from)| *from == got[0].1),
            "the queries came from more than one socket: {got:?}"
        );
        for (query, from) in got.iter().rev() {
            host.send_to(query, from).unwrap();
        }
        let mut answers = vec![None; handles.len()];
        guest.until("every answer has come back", |sockets| {
            for (answer, &handle) in answers.iter_mut().zip(&handles) {
                if let Ok((data, _)) = sockets.get_mut::<udp::Socket>(handle).recv() {
                    *answer = Some(data.to_vec());
                }
            }
            answers.iter().all(Option::is_some)
        });
        for (i, answer) in answers.into_iter().enumerate() {
            assert_eq!(answer.unwrap(), [0, 1, i as u8]);
        }
    }

    /// A connection to a published port reaches the guest's listener on that port at its
    /// loopback, from the gateway, without a network.
    #[test]
    fn a_published_port_reaches_the_guests_loopback_listener() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let none = Egress {
            enabled: false,
            resolver: None,
            client: None,
            outward: None,
        };
        let mut guest = Guest::new(none, vec![(8080, listener)]);
        let buffer = || tcp::SocketBuffer::new(vec![0; 1 << 16]);
        let mut socket = tcp::Socket::new(buffer(), buffer());
        socket
            .listen((IpAddress::from(Ipv4Addr::LOCALHOST), 8080))
            .unwrap();
        let handle = guest.sockets.add(socket);

        let client = thread::spawn(move || {
            let mut client = TcpStream::connect(address).unwrap();
            client.write_all(b"hello").unwrap();
            let mut got = [0; 5];
            client.read_exact(&mut got).unwrap();
            got
        });
        let mut got = Vec::new();
        guest.until("the guest has the client's bytes", |sockets| {
            let socket = sockets.get_mut::<tcp::Socket>(handle);
            if socket.can_recv() {
                socket
                    .recv(|buf| (buf.len(), got.extend_from_slice(buf)))
                    .unwrap();
            }
            got.len() == 5
        });
        assert_eq!(got, b"hello");
        let socket = guest.sockets.get_mut::<tcp::Socket>(handle);
        assert_eq!(
            socket.remote_endpoint().map(|end| end.addr),
            Some(IpAddress::from(LAN.gateway))
        );
        socket.send_slice(b"world").unwrap();
        guest.until("the guest's reply is out", |sockets| {
            sockets.get::<tcp::Socket>(handle).send_queue() == 0
        });
        assert_eq!(&client.join().unwrap(), b"world");
    }
}
