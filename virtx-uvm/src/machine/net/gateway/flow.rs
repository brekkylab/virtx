//! A flow: what the guest sends between two ends, carried on a host socket of its own -- a
//! [TCP connection](TcpFlow) or a [UDP flow](UdpFlow). Every flow is driven the same way, once
//! a turn:
//!
//! ```text
//!   pump    move what each side has to the other, and say what the host socket is waited on for
//!   over    whether it is done, and so put away
//! ```
//!
//! [`Flows`] keeps them, each under the key its host socket is waited on by in the poller, and
//! by its two ends. Each host socket is waited on once at a time (`PollMode::Oneshot`), and
//! asked for again only for what its flow waits on, so a socket the stack cannot keep up with is
//! not reported again until it can.

use std::collections::HashMap;
use std::net::SocketAddrV4;

use polling::{Event, Poller};
use slab::Slab;
use smoltcp::iface::SocketSet;
use socket2::Socket;

use super::link::Link;
use super::tcp::TcpFlow;
use super::udp::UdpFlow;

/// A flow's two ends: the guest's, and the one it sent to.
pub(super) type Ends = (SocketAddrV4, SocketAddrV4);

pub(super) enum Flow {
    Tcp(TcpFlow),
    Udp(UdpFlow),
}

/// What a flow is pumped with: smoltcp's sockets, where a TCP connection's guest side is, and
/// the link, which a UDP flow writes what comes back into.
pub(super) struct Turn<'a> {
    pub(super) sockets: &'a mut SocketSet<'static>,
    pub(super) link: &'a mut Link,
}

/// What a flow's host socket is waited on for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Interest {
    pub(super) read: bool,
    pub(super) write: bool,
}

impl Flow {
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
    ends: Ends,
    /// What the poller has been asked to report, and not yet reported.
    armed: Interest,
    ready: bool,
}

#[derive(Default)]
pub(super) struct Flows {
    entries: Slab<Entry>,
    by_ends: HashMap<Ends, usize>,
}

impl Flows {
    pub(super) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The key of the flow between `ends`, if there is one.
    pub(super) fn find(&self, ends: &Ends) -> Option<usize> {
        self.by_ends.get(ends).copied()
    }

    pub(super) fn get_mut(&mut self, key: usize) -> Option<&mut Flow> {
        self.entries.get_mut(key).map(|entry| &mut entry.flow)
    }

    /// Keep `flow` between `ends`, its host socket waited on by `poller` under the key this
    /// answers. A flow the poller will not take is closed, and `None`.
    pub(super) fn insert(
        &mut self,
        poller: &Poller,
        sockets: &mut SocketSet<'static>,
        ends: Ends,
        flow: Flow,
    ) -> Option<usize> {
        let key = self.entries.vacant_key();
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
    pub(super) fn ready(&mut self, key: usize) {
        if let Some(entry) = self.entries.get_mut(key) {
            entry.armed = Interest::default();
            entry.ready = true;
        }
    }

    /// Pump every flow, and ask the poller for what each now waits on.
    pub(super) fn pump(&mut self, poller: &Poller, mut turn: Turn) {
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
    pub(super) fn reap(&mut self, poller: &Poller, sockets: &mut SocketSet<'static>) {
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
