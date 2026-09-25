//! The loop that is the guest's entire network.
//!
//! # What it is made of
//!
//! One thread, one socket, and two halves that do not overlap:
//!
//! ```text
//!   frame in ─┬─ ARP, TCP ──► smoltcp ──► per-flow TcpStream on the host
//!             ├─ UDP ───────► here ─────► per-flow UdpSocket on the host
//!             └─ anything else, dropped
//! ```
//!
//! **TCP goes through smoltcp** because TCP is a protocol with state, and the guest's end
//! of a connection has to be terminated by something that implements it properly --
//! windows, retransmits, the lot. What the host sees is an ordinary `connect`, so what
//! the guest gets is the host's own stack the moment its bytes leave this process.
//!
//! **UDP does not**, because it has no state to terminate and because the binding smoltcp
//! would need is the wrong shape: a socket there is bound to one local endpoint, and what
//! this stack has to answer is *every* endpoint the guest addresses. Reading a datagram
//! off the frame and writing the answer back onto one is a dozen lines and no bookkeeping.
//! DHCP falls out of the same path, which is why there is no server process anywhere.
//!
//! # Why the destinations are not the stack's own address
//!
//! [`set_any_ip`](Interface::set_any_ip) is what makes that work: the interface takes
//! packets addressed to anywhere as though they were addressed to it, so a guest that
//! connects to a public address is talking to a socket on this interface. Without it
//! there would have to be a translation layer rewriting every address in both directions,
//! and the guest would be talking to a lie about where it was connected.
//!
//! # The one ordering that matters
//!
//! A SYN cannot be handed to smoltcp before there is a socket listening for it, or it is
//! answered with a reset. So frames are classified on the way in and a flow is opened
//! *first*, in the same pass, before the frame reaches the interface. Everything else
//! about the loop is arbitrary; this is not.
//!
//! # What ends a flow
//!
//! A TCP flow ends when both halves have closed, or when either end resets. A UDP flow
//! has nothing that ends it, so it ends by going quiet -- [`UDP_IDLE`] with nothing said
//! either way and its socket is closed, which is the only thing keeping a session that
//! resolves a thousand names from holding a thousand sockets.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::sync::Arc;
use std::time::{Duration, Instant as HostInstant};

use smoltcp::iface::{Config, Interface, PollResult, SocketHandle, SocketSet};
use smoltcp::phy::{ChecksumCapabilities, Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::socket::tcp;
use smoltcp::time::Instant;
use smoltcp::wire::{
    EthernetAddress, EthernetFrame, EthernetProtocol, EthernetRepr, HardwareAddress, IpAddress,
    IpCidr, IpProtocol, Ipv4Cidr, Ipv4Packet, Ipv4Repr, TcpPacket, UdpPacket, UdpRepr,
};
use tokio::sync::Notify;

use super::policy::{DNS_PORT, Policy, Reach};
use super::proxy::{self, FromHost, Host, MAX_DATAGRAM};
use super::wire::{self, Wire};
use super::{
    Error, GATEWAY, GATEWAY_MAC, GUEST, GUEST_MAC, MTU, Result, SUBNET_PREFIX, dhcp, resolver,
};

/// Per direction, per TCP flow, inside smoltcp. This is the window the guest is told
/// about, so it is the one number that decides how much is in flight on a connection
/// whose host end is fast and whose guest end is not, or the other way round.
const TCP_BUFFER: usize = 256 * 1024;

/// How long a datagram flow may say nothing before its host socket is closed. Long enough
/// that a retried name lookup reuses the socket it opened, short enough that nothing
/// accumulates.
const UDP_IDLE: Duration = Duration::from_secs(60);

/// How many times one wake may drive smoltcp before the loop moves on. High enough that
/// a full send buffer goes out in one pass -- 256 KiB is 179 frames at this MTU -- and
/// low enough that a flow with more than that to say does not starve the rest.
const POLL_ROUNDS: usize = 256;

/// How long the loop may sleep when neither smoltcp nor a host socket has anything to
/// say. There is nothing to poll for -- every source of work wakes it -- so this is only
/// a ceiling on how stale [`UDP_IDLE`] can get.
const MAX_IDLE: Duration = Duration::from_secs(1);

/// A network, running on a thread of its own for as long as the process lives.
pub(crate) struct Stack;

impl Stack {
    /// Start one, and hand back the virtio-net device on the other end of its wire.
    ///
    /// The thread is not joined and there is no handle to stop it, because there is
    /// nothing that could: [`enter`](crate::session::Session::enter) becomes the machine
    /// and the machine's exit is this process's. A network that outlived its guest would
    /// have to be a network something could still reach, and nothing can -- the other end
    /// of the wire is held by the device and by nothing else.
    pub(crate) fn start(policy: Policy) -> Result<krun::NetDevice> {
        let (pending, device) = wire::pair()?;
        let resolvers = resolver::host_resolvers();

        std::thread::Builder::new()
            .name("cortex-krun-net".to_string())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("a current-thread runtime takes nothing that can be unavailable");

                runtime.block_on(async move {
                    // Inside the runtime and not before it: registering a socket is the
                    // reactor's, and the reactor is only there once something is running
                    // on it.
                    run(Wire::new(pending), policy, resolvers).await
                });
            })
            .map_err(|e| Error::io("starting the thread the guest's network runs on", e))?;

        Ok(device)
    }
}

/// The frames waiting in each direction, which is the whole of what smoltcp sees of the
/// wire.
///
/// A queue rather than the socket itself because the classification has to happen before
/// smoltcp is handed anything -- see the module doc on ordering -- and because what
/// smoltcp emits has to be sendable without the emit itself being able to block.
#[derive(Default)]
struct Queues {
    rx: VecDeque<Vec<u8>>,
    tx: VecDeque<Vec<u8>>,
}

impl Device for Queues {
    type RxToken<'a> = Received;
    type TxToken<'a> = Sending<'a>;

    fn receive(&mut self, _now: Instant) -> Option<(Received, Sending<'_>)> {
        let frame = self.rx.pop_front()?;
        Some((Received(frame), Sending(&mut self.tx)))
    }

    fn transmit(&mut self, _now: Instant) -> Option<Sending<'_>> {
        Some(Sending(&mut self.tx))
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut capabilities = DeviceCapabilities::default();
        capabilities.medium = Medium::Ethernet;
        capabilities.max_transmission_unit = MTU;
        capabilities
    }
}

struct Received(Vec<u8>);

impl RxToken for Received {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(&self.0)
    }
}

struct Sending<'a>(&'a mut VecDeque<Vec<u8>>);

impl TxToken for Sending<'_> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let mut frame = vec![0u8; len];
        let result = f(&mut frame);
        self.0.push_back(frame);
        result
    }
}

/// What names a datagram flow: where it is going, and the guest port it is going from.
/// The port is part of it because a reply has to come back to the socket that asked, and
/// two of the guest's sockets talking to one server are two flows.
type UdpKey = (Ipv4Addr, u16, u16);

/// What names a stream flow, and only for the purpose of not opening a second one for a
/// retransmitted SYN.
type TcpKey = (Ipv4Addr, u16, u16);

struct TcpFlow {
    socket: SocketHandle,
    host: Host,
    key: TcpKey,

    /// Bytes taken off the host and not yet accepted by smoltcp's send buffer. At most
    /// one chunk: the loop takes nothing more from the channel until this is placed, so
    /// the channel is where the backlog sits and the peer feels it.
    pending: Option<Vec<u8>>,

    /// The peer has sent everything it is going to. Kept rather than acted on at once,
    /// because the FIN it becomes may only go out after what is in `pending`.
    eof: bool,

    /// The guest's half is shut, so there is nothing more to hand the peer.
    guest_done: bool,

    /// The connection has been up. Kept because "the guest has stopped sending" and "the
    /// guest has not started yet" look identical from outside -- a socket still in the
    /// handshake may not receive either -- and acting on the second as though it were the
    /// first shuts the peer's half before the guest has said a word.
    talking: bool,
}

struct UdpFlow {
    host: Host,

    /// What the guest addressed, which is what its answers have to appear to come from.
    from: SocketAddrV4,

    /// The guest port to answer on.
    to: u16,

    last: HostInstant,
}

async fn run(mut wire: Wire, policy: Policy, resolvers: Vec<Ipv4Addr>) {
    let started = HostInstant::now();
    let now = || Instant::from_micros(started.elapsed().as_micros() as i64);

    let mut queues = Queues::default();

    let mut config = Config::new(HardwareAddress::Ethernet(EthernetAddress(GATEWAY_MAC)));
    // Nothing here is secret and nothing depends on it being unpredictable from outside,
    // there being no outside: the one peer on this wire is the guest.
    config.random_seed = started.elapsed().as_nanos() as u64;

    let mut iface = Interface::new(config, &mut queues, now());
    iface.update_ip_addrs(|addrs| {
        addrs
            .push(IpCidr::Ipv4(Ipv4Cidr::new(GATEWAY, SUBNET_PREFIX)))
            .expect("one address, on an interface that has room for several");
    });
    // The whole of what lets a guest connect to an address that is not this stack's --
    // see the module doc. It takes both halves: `any_ip` to accept a packet addressed
    // elsewhere, and a route saying where "elsewhere" would have gone, because smoltcp
    // only takes such a packet when it would have been the one forwarding it. The route
    // is to this interface's own address, which is to say: everything ends here.
    iface.set_any_ip(true);
    iface
        .routes_mut()
        .add_default_ipv4_route(GATEWAY)
        .expect("one route, on a table that has room for several");

    let mut sockets = SocketSet::new(Vec::new());
    let mut tcp_flows: Vec<TcpFlow> = Vec::new();
    let mut tcp_open: HashMap<TcpKey, ()> = HashMap::new();
    let mut udp_flows: HashMap<UdpKey, UdpFlow> = HashMap::new();

    let wake = Arc::new(Notify::new());
    let mut arrived: VecDeque<Vec<u8>> = VecDeque::new();

    loop {
        // 1. Everything the guest has said since the last pass, classified -- and, for a
        //    SYN, its flow opened before the frame goes any further.
        wire.drain(&mut arrived);
        while let Some(frame) = arrived.pop_front() {
            classify(
                &frame,
                &policy,
                &resolvers,
                &wake,
                &mut queues,
                &mut sockets,
                &mut tcp_flows,
                &mut tcp_open,
                &mut udp_flows,
            );
        }

        // 2. smoltcp: ARP answered, segments acknowledged, buffers filled and drained.
        //
        // **Until it stops making progress, rather than once.** A `poll` emits at most one
        // frame per socket, so one call per wake makes the throughput the wake rate times
        // the MTU -- measured at 190 frames a second, which is 0.25 MB/s whatever the peer
        // can do, with the send buffer sitting full at 256 KiB the whole time. Draining it
        // here is what smoltcp asks of a caller, and on the same path it is the difference
        // between 0.25 MB/s and 48 MB/s.
        //
        // Bounded, because the sockets are the guest's to open: with no ceiling, one flow
        // with a full buffer holds this loop for as long as it has frames, and the wire,
        // the host sockets and every other flow wait behind it.
        for _ in 0..POLL_ROUNDS {
            if iface.poll(now(), &mut queues, &mut sockets) == PollResult::None {
                break;
            }
        }

        // 3. Both directions of every stream flow, and what it takes to end one.
        pump_tcp(&mut sockets, &mut tcp_flows, &mut tcp_open);

        // 4. Answers to datagrams, which go straight onto the wire.
        pump_udp(&mut udp_flows, &mut queues);

        // 5. Whatever any of that produced. A frame the wire will not take is dropped
        //    rather than retried, which is what every ethernet has always done with one
        //    -- and what the protocols on top of it are already written to survive.
        while let Some(frame) = queues.tx.pop_front() {
            wire.send(&frame);
        }
        wire.flush();

        // 6. Sleep until something happens: a frame, a host socket, or smoltcp's own
        //    timer. `MAX_IDLE` is the ceiling, so an idle flow is reaped on time.
        let delay = iface
            .poll_delay(now(), &sockets)
            .map(|delay| Duration::from_micros(delay.total_micros()))
            .unwrap_or(MAX_IDLE)
            .min(MAX_IDLE);

        tokio::select! {
            _ = wire.ready() => {}
            _ = wake.notified() => {}
            _ = tokio::time::sleep(delay) => {}
        }
    }
}

/// One frame from the guest: what it is, and what has to exist before smoltcp sees it.
#[allow(clippy::too_many_arguments)]
fn classify(
    frame: &[u8],
    policy: &Policy,
    resolvers: &[Ipv4Addr],
    wake: &Arc<Notify>,
    queues: &mut Queues,
    sockets: &mut SocketSet<'static>,
    tcp_flows: &mut Vec<TcpFlow>,
    tcp_open: &mut HashMap<TcpKey, ()>,
    udp_flows: &mut HashMap<UdpKey, UdpFlow>,
) {
    let Ok(ethernet) = EthernetFrame::new_checked(frame) else {
        return;
    };

    match ethernet.ethertype() {
        // smoltcp's, and the only reason it needs to see a frame that is not TCP: the
        // guest has to be able to resolve the gateway before it can send anything to it.
        EthernetProtocol::Arp => queues.rx.push_back(frame.to_vec()),
        EthernetProtocol::Ipv4 => {}
        // IPv6 included. Nothing here answers it, and a guest that gets no answer falls
        // back to v4 -- which is what it would do behind any v4-only network.
        _ => return,
    }
    if ethernet.ethertype() != EthernetProtocol::Ipv4 {
        return;
    }

    let Ok(ip) = Ipv4Packet::new_checked(ethernet.payload()) else {
        return;
    };
    // Fragments are not reassembled, so the first one would be delivered as a whole
    // datagram and the rest silently dropped -- worse than dropping all of it.
    if ip.more_frags() || ip.frag_offset() != 0 {
        return;
    }
    let destination = ip.dst_addr();

    match ip.next_header() {
        IpProtocol::Tcp => {
            let Ok(segment) = TcpPacket::new_checked(ip.payload()) else {
                return;
            };
            // Only a fresh connection needs anything set up; everything after it is
            // smoltcp's business with a socket that already exists.
            if segment.syn() && !segment.ack() {
                let key = (destination, segment.dst_port(), segment.src_port());
                if !tcp_open.contains_key(&key)
                    && let Some(target) = target(policy, destination, segment.dst_port(), false)
                {
                    open_tcp(target, key, wake, sockets, tcp_flows, tcp_open);
                }
            }
            queues.rx.push_back(frame.to_vec());
        }
        IpProtocol::Udp => {
            let Ok(datagram) = UdpPacket::new_checked(ip.payload()) else {
                return;
            };
            udp(
                destination,
                &datagram,
                policy,
                resolvers,
                wake,
                queues,
                udp_flows,
            );
        }
        // ICMP included: answering an echo would take a raw socket, which an
        // unprivileged host process does not get. A `ping` from the guest goes nowhere.
        _ => {}
    }
}

/// Where a flow's host socket goes, or `None` if it does not get one.
fn target(policy: &Policy, addr: Ipv4Addr, port: u16, udp: bool) -> Option<SocketAddrV4> {
    match policy.reach(addr, port, udp) {
        Reach::Outside => Some(SocketAddrV4::new(addr, port)),
        // On loopback and nowhere else: a granted port is a service on *this* machine,
        // and resolving it through an interface would be reaching whatever answers that
        // port on the network this host happens to be on.
        Reach::Host => Some(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port)),
        // Its target is the host's own resolver, which the caller has in hand.
        Reach::Resolver => None,
        Reach::Refused => None,
    }
}

/// A listening socket for a SYN that has not arrived yet, and the connection that will be
/// paired with it -- see the module doc on ordering.
fn open_tcp(
    target: SocketAddrV4,
    key: TcpKey,
    wake: &Arc<Notify>,
    sockets: &mut SocketSet<'static>,
    tcp_flows: &mut Vec<TcpFlow>,
    tcp_open: &mut HashMap<TcpKey, ()>,
) {
    let mut socket = tcp::Socket::new(
        tcp::SocketBuffer::new(vec![0u8; TCP_BUFFER]),
        tcp::SocketBuffer::new(vec![0u8; TCP_BUFFER]),
    );
    // What the guest addressed, which `any_ip` makes an address of this interface's.
    if socket.listen((IpAddress::from(key.0), key.1)).is_err() {
        return;
    }
    // A connection nobody is speaking on is one whose host end may already be gone, and
    // holding it open costs a socket on both sides for as long as the session runs.
    socket.set_keep_alive(Some(smoltcp::time::Duration::from_secs(60)));

    tcp_open.insert(key, ());
    tcp_flows.push(TcpFlow {
        socket: sockets.add(socket),
        host: proxy::tcp(target, wake.clone()),
        key,
        pending: None,
        eof: false,
        guest_done: false,
        talking: false,
    });
}

/// One datagram from the guest: DHCP, a name to look up, or something to send out.
#[allow(clippy::too_many_arguments)]
fn udp(
    destination: Ipv4Addr,
    datagram: &UdpPacket<&[u8]>,
    policy: &Policy,
    resolvers: &[Ipv4Addr],
    wake: &Arc<Notify>,
    queues: &mut Queues,
    udp_flows: &mut HashMap<UdpKey, UdpFlow>,
) {
    let (from, to) = (datagram.src_port(), datagram.dst_port());

    // Answered here and now, from constants -- the client gives up after a tenth of a
    // second, so there is nothing this may wait for. See `dhcp`.
    if to == dhcp::SERVER_PORT {
        if let Some(reply) = dhcp::reply(datagram.payload()) {
            // Broadcast both ways: the guest has no address yet, so there is nothing to
            // address the answer to but everyone.
            queues.tx.push_back(udp_frame(
                SocketAddrV4::new(GATEWAY, dhcp::SERVER_PORT),
                SocketAddrV4::new(Ipv4Addr::BROADCAST, dhcp::CLIENT_PORT),
                EthernetAddress::BROADCAST,
                &reply,
            ));
        }
        return;
    }

    let key = (destination, to, from);
    if let Some(flow) = udp_flows.get_mut(&key) {
        flow.last = HostInstant::now();
        flow.host.send(datagram.payload().to_vec());
        return;
    }

    // A flow's host socket, which for the resolver is the host's own resolver and for
    // everything else is what the guest addressed.
    let target = match policy.reach(destination, to, true) {
        Reach::Resolver => match resolvers.first() {
            Some(resolver) => SocketAddrV4::new(*resolver, DNS_PORT),
            // A host with no resolver of its own -- see `resolver`, which has already
            // said so once. Dropping is what a query to a nameserver that is not there
            // gets anywhere else.
            None => return,
        },
        Reach::Outside => SocketAddrV4::new(destination, to),
        Reach::Host | Reach::Refused => return,
    };

    let host = proxy::udp(target, wake.clone());
    host.send(datagram.payload().to_vec());
    udp_flows.insert(
        key,
        UdpFlow {
            host,
            from: SocketAddrV4::new(destination, to),
            to: from,
            last: HostInstant::now(),
        },
    );
}

/// Both directions of every stream flow, and the three ways one ends.
fn pump_tcp(
    sockets: &mut SocketSet<'static>,
    tcp_flows: &mut Vec<TcpFlow>,
    tcp_open: &mut HashMap<TcpKey, ()>,
) {
    tcp_flows.retain_mut(|flow| {
        let socket = sockets.get_mut::<tcp::Socket>(flow.socket);

        // The guest's bytes, as far as the host channel will take them. What does not fit
        // stays in the receive buffer, which closes the window -- the guest is told to
        // wait in the only way TCP has of saying it.
        while socket.can_recv() {
            let Some(permit) = flow.host.reserve() else {
                break;
            };
            let taken = socket.recv(|bytes| (bytes.len(), bytes.to_vec()));
            match taken {
                Ok(bytes) if !bytes.is_empty() => permit.send(bytes),
                _ => break,
            }
        }

        // The peer's bytes. `pending` is what smoltcp would not take last time.
        if flow.pending.is_none() {
            match flow.host.incoming.try_recv() {
                Ok(FromHost::Data(bytes)) => flow.pending = Some(bytes),
                Ok(FromHost::Eof) => flow.eof = true,
                Ok(FromHost::Failed) => {
                    // No connection, and so nothing to close gracefully: a reset is what
                    // the host itself would have sent.
                    socket.abort();
                    tcp_open.remove(&flow.key);
                    return false;
                }
                Err(_) => {}
            }
        }
        if let Some(bytes) = flow.pending.take() {
            match socket.send_slice(&bytes) {
                Ok(sent) if sent < bytes.len() => flow.pending = Some(bytes[sent..].to_vec()),
                Ok(_) => {}
                Err(_) => flow.pending = Some(bytes),
            }
        }

        // The peer is done and everything it sent has been handed over, so the guest can
        // be told -- but not before, or the FIN would overtake the bytes it follows.
        if flow.eof && flow.pending.is_none() && socket.send_queue() == 0 {
            socket.close();
        }

        // The guest is done, so the peer's socket is shut for writing -- which is what
        // dropping the only sender does, in the task that holds it. Only once the
        // connection has been up, or a socket still shaking hands would be read as one
        // that had finished -- see `talking`.
        flow.talking |= socket.may_recv();
        if !flow.guest_done && flow.talking && !socket.may_recv() {
            flow.guest_done = true;
            flow.host.finish();
        }

        // Gone means gone: the handles are dropped, which ends the host task, and the
        // key stops standing in the way of the guest opening the same flow again.
        let done = !socket.is_active() && !socket.is_listening();
        if done {
            sockets.remove(flow.socket);
            tcp_open.remove(&flow.key);
        }
        !done
    });
}

/// Every datagram flow's answers, and the flows that have stopped having any.
fn pump_udp(udp_flows: &mut HashMap<UdpKey, UdpFlow>, queues: &mut Queues) {
    let now = HostInstant::now();

    udp_flows.retain(|_, flow| {
        loop {
            match flow.host.incoming.try_recv() {
                Ok(FromHost::Data(payload)) => {
                    // No fragmentation here, so a datagram that will not fit in a frame
                    // is one the guest cannot be given. Dropping it is what a link with a
                    // smaller MTU would have done to it further along.
                    if payload.len() > MAX_DATAGRAM {
                        continue;
                    }
                    flow.last = now;
                    queues.tx.push_back(udp_frame(
                        flow.from,
                        SocketAddrV4::new(GUEST, flow.to),
                        EthernetAddress(GUEST_MAC),
                        &payload,
                    ));
                }
                Ok(FromHost::Eof | FromHost::Failed) => return false,
                Err(_) => break,
            }
        }
        now.duration_since(flow.last) < UDP_IDLE
    });
}

/// One ethernet frame carrying one datagram, built rather than routed: both addresses are
/// this stack's to choose and the guest's hardware address is one it was handed.
fn udp_frame(
    from: SocketAddrV4,
    to: SocketAddrV4,
    hardware: EthernetAddress,
    payload: &[u8],
) -> Vec<u8> {
    let checksums = ChecksumCapabilities::default();

    let udp = UdpRepr {
        src_port: from.port(),
        dst_port: to.port(),
    };
    let ip = Ipv4Repr {
        src_addr: *from.ip(),
        dst_addr: *to.ip(),
        next_header: IpProtocol::Udp,
        payload_len: udp.header_len() + payload.len(),
        hop_limit: 64,
    };
    let ethernet = EthernetRepr {
        src_addr: EthernetAddress(GATEWAY_MAC),
        dst_addr: hardware,
        ethertype: EthernetProtocol::Ipv4,
    };

    // Every header, and then the payload. `Ipv4Repr::buffer_len` is its own header only,
    // so the UDP one has to be counted here or the datagram runs off the end of the frame.
    let mut frame =
        vec![0u8; ethernet.buffer_len() + ip.buffer_len() + udp.header_len() + payload.len()];
    ethernet.emit(&mut EthernetFrame::new_unchecked(&mut frame[..]));

    let ip_at = ethernet.buffer_len();
    ip.emit(
        &mut Ipv4Packet::new_unchecked(&mut frame[ip_at..]),
        &checksums,
    );

    let udp_at = ip_at + ip.buffer_len();
    udp.emit(
        &mut UdpPacket::new_unchecked(&mut frame[udp_at..]),
        &IpAddress::Ipv4(ip.src_addr),
        &IpAddress::Ipv4(ip.dst_addr),
        payload.len(),
        |into| into.copy_from_slice(payload),
        &checksums,
    );

    frame
}
