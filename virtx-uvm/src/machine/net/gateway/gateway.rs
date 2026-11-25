use std::io;
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener};
use std::ops::Range;
use std::time::Duration;

use polling::{Event, Events, Poller};
use smoltcp::iface::{Config, Interface, PollResult, Route, SocketSet};
use smoltcp::time::Instant;
use smoltcp::wire::{
    EthernetAddress, EthernetFrame, EthernetProtocol, IpCidr, IpProtocol, Ipv4Packet, TcpPacket,
    UdpPacket,
};

use super::LAN;
use super::egress::Egress;
use super::flow::{Ends, Flow, Flows, Turn};
use super::link::Link;
use super::tcp::TcpFlow;
use super::udp::{self, UdpFlow};

/// The link's key in the poller, below the one the poller keeps for itself. A published port's
/// listener is under it, by its place; a flow's is its own key in [`Flows`].
const LINK: usize = usize::MAX - 1;

/// The first port the gateway's end of a connection to a published port takes.
const EPHEMERAL: u16 = 49152;

pub(in crate::machine::net) struct Gateway {
    // First, so it is dropped first: every source it waits on is still open when it goes.
    poller: Poller,
    link: Link,
    iface: Interface,
    sockets: SocketSet<'static>,
    egress: Egress,
    flows: Flows,
    published: Vec<(u16, TcpListener)>,
    next_port: u16,
    started: std::time::Instant,
}

impl Gateway {
    pub(in crate::machine::net) fn new(
        link: Link,
        egress: bool,
        published: Vec<(u16, TcpListener)>,
    ) -> io::Result<Self> {
        Self::with_egress(link, Egress::new(egress), published)
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
            unsafe { poller.add(listener, Event::readable(LINK - 1 - i))? };
        }
        Ok(Gateway {
            poller,
            link,
            iface,
            sockets: SocketSet::new(Vec::new()),
            egress,
            flows: Flows::default(),
            published,
            next_port: EPHEMERAL,
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
            if !self.flows.is_empty() {
                timeout = Some(timeout.map_or(udp::IDLE, |timeout| timeout.min(udp::IDLE)));
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
                    key if key >= LINK - self.published.len() => self.accept(LINK - 1 - key)?,
                    key => self.flows.ready(key),
                }
            }

            let now = self.now();
            if link_ready && !self.link.fill()? {
                return Ok(());
            }
            self.ingress(now);
            self.flows.pump(
                &self.poller,
                Turn {
                    sockets: &mut self.sockets,
                    link: &mut self.link,
                },
            );
            self.iface.poll_maintenance(now);
            while self
                .iface
                .poll_egress(now, &mut self.link, &mut self.sockets)
                != PollResult::None
            {}
            self.link.flush()?;
            self.flows.reap(&self.poller, &mut self.sockets);
        }
    }

    fn now(&self) -> Instant {
        Instant::from_micros(self.started.elapsed().as_micros() as i64)
    }

    /// Take every frame read in: open the flow a frame opens, carry UDP, give smoltcp the rest.
    fn ingress(&mut self, now: Instant) {
        while let Some(frame) = self.link.peek() {
            match Frame::of(frame) {
                Frame::Udp { ends, payload } => {
                    let key = self.flows.find(&ends).or_else(|| {
                        let at = self.egress.route(ends.1)?;
                        let flow = Flow::Udp(UdpFlow::open(ends, at).ok()?);
                        self.flows
                            .insert(&self.poller, &mut self.sockets, ends, flow)
                    });
                    if let Some(Flow::Udp(udp)) = key.and_then(|key| self.flows.get_mut(key))
                        && let Some(frame) = self.link.peek()
                    {
                        udp.send(&frame[payload]);
                    }
                    self.link.skip();
                }
                Frame::Syn { ends } => {
                    if self.flows.find(&ends).is_none()
                        && let Some(at) = self.egress.route(ends.1)
                        && let Ok(tcp) = TcpFlow::open(&mut self.sockets, ends.1, at)
                    {
                        self.flows
                            .insert(&self.poller, &mut self.sockets, ends, Flow::Tcp(tcp));
                    }
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
        let guest = SocketAddrV4::new(Ipv4Addr::LOCALHOST, port);
        loop {
            let host = match self.published[i].1.accept() {
                Ok((host, _)) => host,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            };
            let gateway = loop {
                let gateway = SocketAddrV4::new(LAN.gateway, self.next_port);
                self.next_port = self.next_port.checked_add(1).unwrap_or(EPHEMERAL);
                if self.flows.find(&(guest, gateway)).is_none() {
                    break gateway;
                }
            };
            if let Ok(tcp) = TcpFlow::accept(
                &mut self.sockets,
                self.iface.context(),
                host.into(),
                guest,
                gateway,
            ) {
                self.flows.insert(
                    &self.poller,
                    &mut self.sockets,
                    (guest, gateway),
                    Flow::Tcp(tcp),
                );
            }
        }
        self.poller
            .modify(&self.published[i].1, Event::readable(LINK - 1 - i))
    }
}

/// What a frame from the guest is, as far as the stack takes it apart.
enum Frame {
    /// A TCP connection's first segment.
    Syn { ends: Ends },
    /// A datagram, its payload where it lies in the frame.
    Udp { ends: Ends, payload: Range<usize> },
    /// smoltcp's: ARP, the rest of TCP, and ICMP to this end.
    Smoltcp,
    /// Nothing to answer. ICMP to anywhere but this end is dropped rather than left to smoltcp,
    /// which answers for every address and would echo a ping to one the guest cannot reach.
    Drop,
}

impl Frame {
    fn of(frame: &[u8]) -> Frame {
        let Ok(ethernet) = EthernetFrame::new_checked(frame) else {
            return Frame::Drop;
        };
        if ethernet.ethertype() != EthernetProtocol::Ipv4 {
            return Frame::Smoltcp;
        }
        let Ok(ip) = Ipv4Packet::new_checked(ethernet.payload()) else {
            return Frame::Drop;
        };
        let (src, dst) = (ip.src_addr(), ip.dst_addr());
        let fragment = ip.more_frags() || ip.frag_offset() != 0;
        match ip.next_header() {
            IpProtocol::Tcp if !fragment => match TcpPacket::new_checked(ip.payload()) {
                Ok(tcp) if tcp.syn() && !tcp.ack() => Frame::Syn {
                    ends: (
                        SocketAddrV4::new(src, tcp.src_port()),
                        SocketAddrV4::new(dst, tcp.dst_port()),
                    ),
                },
                Ok(_) => Frame::Smoltcp,
                Err(_) => Frame::Drop,
            },
            IpProtocol::Udp if !fragment => match UdpPacket::new_checked(ip.payload()) {
                Ok(udp) => {
                    let at = EthernetFrame::<&[u8]>::header_len() + ip.header_len() as usize;
                    Frame::Udp {
                        ends: (
                            SocketAddrV4::new(src, udp.src_port()),
                            SocketAddrV4::new(dst, udp.dst_port()),
                        ),
                        payload: at + 8..at + udp.len() as usize,
                    }
                }
                Err(_) => Frame::Drop,
            },
            IpProtocol::Icmp if dst == LAN.gateway || dst == LAN.nameserver => Frame::Smoltcp,
            _ => Frame::Drop,
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::io::{Read as _, Write as _};
    use std::net::{SocketAddr, TcpStream, UdpSocket};
    use std::os::unix::net::UnixStream;
    use std::thread;
    use std::time::Instant as Clock;

    use smoltcp::iface::SocketHandle;
    use smoltcp::socket::{tcp, udp};
    use smoltcp::wire::IpAddress;

    use super::*;

    /// A guest at the far end of a stack's link: smoltcp of its own, at the guest's address and
    /// its loopback, routed through the gateway.
    struct Guest {
        link: Link,
        iface: Interface,
        sockets: SocketSet<'static>,
        started: Clock,
    }

    impl Guest {
        fn new(egress: Egress, published: Vec<(u16, TcpListener)>) -> Guest {
            let (ours, theirs) = UnixStream::pair().unwrap();
            let stack =
                Gateway::with_egress(Link::new(ours.into()).unwrap(), egress, published).unwrap();
            thread::spawn(move || stack.run());
            let mut link = Link::new(theirs.into()).unwrap();
            let config = Config::new(EthernetAddress(LAN.guest_mac).into());
            let mut iface = Interface::new(config, &mut link, Instant::ZERO);
            iface.update_ip_addrs(|addrs| {
                addrs
                    .push(IpCidr::new(LAN.guest.into(), LAN.prefix))
                    .unwrap();
                addrs
                    .push(IpCidr::new(Ipv4Addr::LOCALHOST.into(), 8))
                    .unwrap();
            });
            iface
                .routes_mut()
                .add_default_ipv4_route(LAN.gateway)
                .unwrap();
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
                let now = Instant::from_micros(self.started.elapsed().as_micros() as i64);
                self.link.fill().unwrap();
                self.iface.poll(now, &mut self.link, &mut self.sockets);
                self.link.flush().unwrap();
                thread::sleep(Duration::from_millis(1));
            }
        }

        fn connect(&mut self, to: SocketAddrV4) -> SocketHandle {
            let buffer = || tcp::SocketBuffer::new(vec![0; 256 << 10]);
            let mut socket = tcp::Socket::new(buffer(), buffer());
            let to = (IpAddress::from(*to.ip()), to.port());
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
        }
    }

    const DNS: SocketAddrV4 = SocketAddrV4::new(LAN.nameserver, 53);

    /// A connection carries a megabyte each way, and each side's close reaches the other.
    #[test]
    fn a_connection_carries_both_ways_and_ends_from_each_side() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let mut guest = Guest::new(resolved_at(listener.local_addr().unwrap()), vec![]);
        let handle = guest.connect(DNS);
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

    /// A connection the policy sends nowhere -- this host, at the gateway -- is reset.
    #[test]
    fn a_connection_the_policy_refuses_is_reset() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut guest = Guest::new(resolved_at(listener.local_addr().unwrap()), vec![]);
        let handle = guest.connect(SocketAddrV4::new(LAN.gateway, port));
        guest.until("the connection is reset", |sockets| {
            sockets.get::<tcp::Socket>(handle).state() == tcp::State::Closed
        });
    }

    /// A datagram reaches where the policy says, and the reply comes back from where the guest
    /// sent it.
    #[test]
    fn a_datagram_goes_out_and_its_reply_comes_back_from_where_it_was_sent() {
        let host = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let mut guest = Guest::new(resolved_at(host.local_addr().unwrap()), vec![]);
        let buffer =
            || udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 4], vec![0; 1 << 16]);
        let mut socket = udp::Socket::new(buffer(), buffer());
        socket.bind(40000).unwrap();
        let handle = guest.sockets.add(socket);
        let to = (IpAddress::from(*DNS.ip()), DNS.port());
        guest
            .sockets
            .get_mut::<udp::Socket>(handle)
            .send_slice(b"ping", to)
            .unwrap();

        guest.until("the guest's datagram is out", |sockets| {
            sockets.get::<udp::Socket>(handle).send_queue() == 0
        });
        let mut buf = [0; 16];
        let (n, from) = host.recv_from(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"ping");
        host.send_to(b"pong", from).unwrap();
        let mut reply = None;
        guest.until("the reply has come back", |sockets| {
            if let Ok((data, meta)) = sockets.get_mut::<udp::Socket>(handle).recv() {
                reply = Some((data.to_vec(), meta.endpoint));
            }
            reply.is_some()
        });
        let (data, endpoint) = reply.unwrap();
        assert_eq!(data, b"pong");
        assert_eq!(endpoint, to.into());
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
