//! The nameserver: every query the guest sends it goes to this host's resolver on one socket,
//! its ID swapped for one of the socket's own, and each answer back to where its query came
//! from with the query's ID -- one host socket for every lookup, however many ports they are
//! sent from.

use std::collections::HashMap;
use std::hash::{BuildHasher, RandomState};
use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use polling::{Event, Poller};

use super::flow::Ends;
use super::link::Link;

/// The most queries waiting for an answer; one more is dropped, and the guest asks again.
const MAX_PENDING: usize = 256;

/// How long a query waits for its answer: as long as a guest's resolver does before it asks
/// again.
const TIMEOUT: Duration = Duration::from_secs(5);

pub(super) struct Dns {
    /// Connected to the resolver; `None` where queries go nowhere.
    host: Option<UdpSocket>,
    /// Its key in the poller.
    key: usize,
    /// Each query waiting, by the ID it was sent on with: where it came from, its own ID, and
    /// when it was sent.
    pending: HashMap<u16, (Ends, u16, Instant)>,
    /// What the IDs sent on are drawn from, so the resolver's answers cannot be guessed at.
    ids: RandomState,
    sent: u64,
    buf: Box<[u8]>,
    /// What the poller has been asked to report, and not yet reported.
    armed: bool,
    ready: bool,
}

impl Dns {
    /// Queries sent on to `resolver`, its socket waited on by `poller` under `key`. A resolver
    /// this host cannot send to gets none.
    pub(super) fn new(poller: &Poller, key: usize, resolver: Option<SocketAddr>) -> Dns {
        let open = |at: SocketAddr| -> io::Result<UdpSocket> {
            let any: SocketAddr = match at {
                SocketAddr::V4(_) => (Ipv4Addr::UNSPECIFIED, 0).into(),
                SocketAddr::V6(_) => (Ipv6Addr::UNSPECIFIED, 0).into(),
            };
            let host = UdpSocket::bind(any)?;
            host.connect(at)?;
            host.set_nonblocking(true)?;
            // SAFETY: the poller is dropped before the stack's other fields, this among them.
            unsafe { poller.add(&host, Event::none(key))? };
            Ok(host)
        };
        Dns {
            host: resolver.and_then(|at| open(at).ok()),
            key,
            pending: HashMap::new(),
            ids: RandomState::new(),
            sent: 0,
            buf: vec![0; u16::MAX as usize].into_boxed_slice(),
            armed: false,
            ready: false,
        }
    }

    /// Send on `query`, which the guest sent between `ends`. One too short to carry an ID, or
    /// past [`MAX_PENDING`], is dropped.
    pub(super) fn query(&mut self, ends: Ends, query: &[u8]) {
        let Some(host) = &self.host else {
            return;
        };
        if query.len() < 2 || self.pending.len() >= MAX_PENDING {
            return;
        }
        let id = loop {
            self.sent += 1;
            let id = self.ids.hash_one(self.sent) as u16;
            if !self.pending.contains_key(&id) {
                break id;
            }
        };
        let buf = &mut self.buf[..query.len()];
        buf.copy_from_slice(query);
        buf[..2].copy_from_slice(&id.to_be_bytes());
        if host.send(buf).is_ok() {
            let own = u16::from_be_bytes([query[0], query[1]]);
            self.pending.insert(id, (ends, own, Instant::now()));
        }
    }

    /// The poller reported the socket, which it then waits on for nothing.
    pub(super) fn ready(&mut self) {
        self.armed = false;
        self.ready = true;
    }

    /// Carry each answer the socket has to the guest, if it was reported, while the link will
    /// take more; and wait on it while the link will.
    pub(super) fn pump(&mut self, poller: &Poller, link: &mut Link) {
        let Some(host) = &self.host else {
            return;
        };
        while self.ready && !link.full() {
            let n = match host.recv(&mut self.buf) {
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                // A query the resolver's host refused, which a connected socket is told of.
                Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => continue,
                Err(_) => break,
            };
            let Some((ends, own, _)) = self.buf[..n]
                .get(..2)
                .and_then(|id| self.pending.remove(&u16::from_be_bytes([id[0], id[1]])))
            else {
                continue;
            };
            self.buf[..2].copy_from_slice(&own.to_be_bytes());
            let answer = &self.buf[..n];
            link.datagram(ends.1, ends.0, |payload| {
                payload.get_mut(..n)?.copy_from_slice(answer);
                Some(n)
            });
        }
        self.ready = false;
        let read = !link.full();
        if read != self.armed
            && poller
                .modify(host, Event::new(self.key, read, false))
                .is_ok()
        {
            self.armed = read;
        }
    }

    /// Forget the queries no answer came for.
    pub(super) fn reap(&mut self) {
        self.pending
            .retain(|_, (_, _, sent)| sent.elapsed() < TIMEOUT);
    }
}
