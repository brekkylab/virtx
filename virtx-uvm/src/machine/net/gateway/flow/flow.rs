use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Duration;

use polling::{Event, Poller};
use slab::Slab;
use smoltcp::iface::{Context, SocketSet};
use smoltcp::wire::IpProtocol;
use socket2::Socket;

use super::super::LAN;
use super::super::link::Link;
use super::tcp::TcpFlow;
use super::udp::{self, UdpFlow};

/// A flow's two ends: the guest's, and the one it sent to.
pub(in crate::machine::net::gateway) type Ends = (SocketAddr, SocketAddr);

/// The first port the gateway's end of a connection to a published port takes.
const EPHEMERAL: u16 = 49152;

enum Flow {
    Tcp(TcpFlow),
    Udp(UdpFlow),
}

/// What a flow's host socket is waited on for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Interest {
    pub(super) read: bool,
    pub(super) write: bool,
}

impl Flow {
    /// What it carries, which with its ends is what tells it from another: TCP and UDP each
    /// have ports of their own.
    fn protocol(&self) -> IpProtocol {
        match self {
            Flow::Tcp(_) => IpProtocol::Tcp,
            Flow::Udp(_) => IpProtocol::Udp,
        }
    }

    fn host(&self) -> &Socket {
        match self {
            Flow::Tcp(tcp) => tcp.host(),
            Flow::Udp(udp) => udp.host(),
        }
    }

    /// `ready` is whether its host socket was reported since the last turn.
    fn pump(&mut self, sockets: &mut SocketSet, link: &mut Link, ready: bool) -> Interest {
        match self {
            Flow::Tcp(tcp) => tcp.pump(sockets, ready),
            Flow::Udp(udp) => udp.pump(link, ready),
        }
    }

    fn over(&self, sockets: &SocketSet) -> bool {
        match self {
            Flow::Tcp(tcp) => tcp.over(sockets),
            Flow::Udp(udp) => udp.over(),
        }
    }

    fn close(self, sockets: &mut SocketSet) {
        if let Flow::Tcp(tcp) = self {
            tcp.close(sockets);
        }
    }
}

struct Entry {
    flow: Flow,
    ends: (IpProtocol, Ends),
    /// What the poller has been asked to report, and not yet reported.
    armed: Interest,
    ready: bool,
}

pub(in crate::machine::net::gateway) struct Flows {
    entries: Slab<Entry>,
    by_ends: HashMap<(IpProtocol, Ends), usize>,
    /// The port the gateway's end of the next connection to a published port tries.
    next_port: u16,
}

impl Default for Flows {
    fn default() -> Flows {
        Flows {
            entries: Slab::new(),
            by_ends: HashMap::new(),
            next_port: EPHEMERAL,
        }
    }
}

impl Flows {
    /// How long the stack may wait before its next turn: as long as a UDP flow may be idle,
    /// while there are flows to put away when they are.
    pub(in crate::machine::net::gateway) fn timeout(&self) -> Option<Duration> {
        (!self.entries.is_empty()).then_some(udp::IDLE)
    }

    /// The guest's SYN between `ends`: a connection opened to where `route` says, unless one is
    /// open between them already. With none, smoltcp finds no socket for the SYN, and resets it.
    pub(in crate::machine::net::gateway) fn syn(
        &mut self,
        poller: &Poller,
        sockets: &mut SocketSet<'static>,
        ends: Ends,
        route: impl FnOnce() -> Option<SocketAddr>,
    ) {
        if self.by_ends.contains_key(&(IpProtocol::Tcp, ends)) {
            return;
        }
        if let Some(at) = route()
            && let Ok(tcp) = TcpFlow::open(sockets, ends.1, at)
            && let Err(flow) = self.insert(poller, ends, Flow::Tcp(tcp))
        {
            flow.close(sockets);
        }
    }

    /// A datagram the guest sent between `ends`, sent on the flow between them -- opened to
    /// where `route` says if there is none. One with nowhere to go is dropped.
    pub(in crate::machine::net::gateway) fn udp(
        &mut self,
        poller: &Poller,
        ends: Ends,
        payload: &[u8],
        route: impl FnOnce() -> Option<SocketAddr>,
    ) {
        let key = match self.by_ends.get(&(IpProtocol::Udp, ends)) {
            Some(&key) => key,
            None => {
                let Some(udp) = route().and_then(|at| UdpFlow::open(ends, at).ok()) else {
                    return;
                };
                let Ok(key) = self.insert(poller, ends, Flow::Udp(udp)) else {
                    return;
                };
                key
            }
        };
        if let Some(Flow::Udp(udp)) = self.entries.get_mut(key).map(|entry| &mut entry.flow) {
            udp.send(payload);
        }
    }

    /// A connection a client of a published port opened, `host`, carried to `guest` from a port
    /// of the gateway's that no other connection to `guest` holds.
    pub(in crate::machine::net::gateway) fn accept(
        &mut self,
        poller: &Poller,
        sockets: &mut SocketSet<'static>,
        cx: &mut Context,
        host: Socket,
        guest: SocketAddr,
    ) {
        let gateway = loop {
            let gateway = SocketAddr::from((LAN.gateway, self.next_port));
            self.next_port = self.next_port.checked_add(1).unwrap_or(EPHEMERAL);
            if !self
                .by_ends
                .contains_key(&(IpProtocol::Tcp, (guest, gateway)))
            {
                break gateway;
            }
        };
        if let Ok(tcp) = TcpFlow::accept(sockets, cx, host, guest, gateway)
            && let Err(flow) = self.insert(poller, (guest, gateway), Flow::Tcp(tcp))
        {
            flow.close(sockets);
        }
    }

    /// Keep `flow` between `ends`, its host socket waited on by `poller` under the key this
    /// answers; or give it back, for a poller that will not take it.
    fn insert(&mut self, poller: &Poller, ends: Ends, flow: Flow) -> Result<usize, Flow> {
        let key = self.entries.vacant_key();
        let ends = (flow.protocol(), ends);
        // SAFETY: deleted from the poller in `reap` before the flow, and its socket, drops --
        // or the poller goes first, with the stack.
        if unsafe { poller.add(flow.host(), Event::none(key)) }.is_err() {
            return Err(flow);
        }
        self.entries.insert(Entry {
            flow,
            ends,
            armed: Interest::default(),
            ready: false,
        });
        self.by_ends.insert(ends, key);
        Ok(key)
    }

    /// The poller reported the host socket under `key`, which it then waits on for nothing.
    pub(in crate::machine::net::gateway) fn ready(&mut self, key: usize) {
        if let Some(entry) = self.entries.get_mut(key) {
            entry.armed = Interest::default();
            entry.ready = true;
        }
    }

    /// Pump every flow, and ask the poller for what each now waits on.
    pub(in crate::machine::net::gateway) fn pump(
        &mut self,
        poller: &Poller,
        sockets: &mut SocketSet<'static>,
        link: &mut Link,
    ) {
        for (key, entry) in self.entries.iter_mut() {
            let interest = entry.flow.pump(sockets, link, entry.ready);
            entry.ready = false;
            if interest != entry.armed {
                let event = Event::new(key, interest.read, interest.write);
                if poller.modify(entry.flow.host(), event).is_ok() {
                    entry.armed = interest;
                }
            }
        }
    }

    /// Put away every flow that is over.
    pub(in crate::machine::net::gateway) fn reap(
        &mut self,
        poller: &Poller,
        sockets: &mut SocketSet<'static>,
    ) {
        let over: Vec<usize> = self
            .entries
            .iter()
            .filter(|(_, entry)| entry.flow.over(sockets))
            .map(|(key, _)| key)
            .collect();
        for key in over {
            let entry = self.entries.remove(key);
            let _ = poller.delete(entry.flow.host());
            self.by_ends.remove(&entry.ends);
            entry.flow.close(sockets);
        }
    }
}
