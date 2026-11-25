use std::collections::HashMap;
use std::net::SocketAddrV4;

use polling::{Event, Poller};
use slab::Slab;
use smoltcp::iface::SocketSet;
use smoltcp::wire::IpProtocol;
use socket2::Socket;

use super::super::link::Link;
use super::tcp::TcpFlow;
use super::udp::UdpFlow;

/// A flow's two ends: the guest's, and the one it sent to.
pub(in crate::machine::net::gateway) type Ends = (SocketAddrV4, SocketAddrV4);

pub(in crate::machine::net::gateway) enum Flow {
    Tcp(TcpFlow),
    Udp(UdpFlow),
}

/// What a flow is pumped with: smoltcp's sockets, where a TCP connection's guest side is, and
/// the link, which a UDP flow writes what comes back into.
pub(in crate::machine::net::gateway) struct Turn<'a> {
    pub(in crate::machine::net::gateway) sockets: &'a mut SocketSet<'static>,
    pub(in crate::machine::net::gateway) link: &'a mut Link,
}

/// What a flow's host socket is waited on for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::machine::net::gateway) struct Interest {
    pub(in crate::machine::net::gateway) read: bool,
    pub(in crate::machine::net::gateway) write: bool,
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
    fn pump(&mut self, turn: &mut Turn, ready: bool) -> Interest {
        match self {
            Flow::Tcp(tcp) => tcp.pump(turn.sockets),
            Flow::Udp(udp) => udp.pump(turn.link, ready),
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

#[derive(Default)]
pub(in crate::machine::net::gateway) struct Flows {
    entries: Slab<Entry>,
    by_ends: HashMap<(IpProtocol, Ends), usize>,
}

impl Flows {
    pub(in crate::machine::net::gateway) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The key of the `protocol` flow between `ends`, if there is one.
    pub(in crate::machine::net::gateway) fn find(
        &self,
        protocol: IpProtocol,
        ends: Ends,
    ) -> Option<usize> {
        self.by_ends.get(&(protocol, ends)).copied()
    }

    pub(in crate::machine::net::gateway) fn get_mut(&mut self, key: usize) -> Option<&mut Flow> {
        self.entries.get_mut(key).map(|entry| &mut entry.flow)
    }

    /// Keep `flow` between `ends`, its host socket waited on by `poller` under the key this
    /// answers. A flow the poller will not take is closed, and `None`.
    pub(in crate::machine::net::gateway) fn insert(
        &mut self,
        poller: &Poller,
        sockets: &mut SocketSet<'static>,
        ends: Ends,
        flow: Flow,
    ) -> Option<usize> {
        let key = self.entries.vacant_key();
        let ends = (flow.protocol(), ends);
        // SAFETY: deleted from the poller in `reap` before the flow, and its socket, drops --
        // or the poller goes first, with the stack.
        if unsafe { poller.add(flow.host(), Event::none(key)) }.is_err() {
            flow.close(sockets);
            return None;
        }
        self.entries.insert(Entry {
            flow,
            ends,
            armed: Interest::default(),
            ready: false,
        });
        self.by_ends.insert(ends, key);
        Some(key)
    }

    /// The poller reported the host socket under `key`, which it then waits on for nothing.
    pub(in crate::machine::net::gateway) fn ready(&mut self, key: usize) {
        if let Some(entry) = self.entries.get_mut(key) {
            entry.armed = Interest::default();
            entry.ready = true;
        }
    }

    /// Pump every flow, and ask the poller for what each now waits on.
    pub(in crate::machine::net::gateway) fn pump(&mut self, poller: &Poller, mut turn: Turn) {
        for (key, entry) in self.entries.iter_mut() {
            let interest = entry.flow.pump(&mut turn, entry.ready);
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
