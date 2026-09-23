//! The host end of a flow: one real socket, opened because the guest asked for one and
//! [`Policy`](super::policy::Policy) allowed it.
//!
//! # Why a task and not a socket in the loop
//!
//! Because the stack's loop has to stay the length of one frame. It holds the whole of
//! the guest's network -- every flow's state, the interface, the queues -- so anything it
//! waits on, every flow waits on. A `connect` to a host that is not answering takes as
//! long as it takes, and a write to a peer that has stopped reading takes forever, and
//! neither is something the guest's other connections have anything to do with.
//!
//! So a flow's socket lives in a task of its own, and what it shares with the loop is two
//! channels and a [`Notify`]. The channels are bounded, which is the whole of the flow
//! control: a host peer that stops reading fills the outbound channel, which stops the
//! loop taking bytes out of the guest's window, which is the guest being told to slow
//! down -- in the one language TCP has for saying it.
//!
//! # The shape is the same for both protocols
//!
//! A [`Host`] is a pair of channels either way, and what a `Vec<u8>` in it means is all
//! that differs: for TCP it is however many bytes came out of the stream, and for UDP it
//! is exactly one datagram. The stack keeps that straight because it knows which kind of
//! flow it opened; nothing here has to.

use std::net::SocketAddrV4;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::{Notify, mpsc};

use super::MTU;

/// How much is taken off a stream at once. A read this size fills a good few frames, so
/// the loop is woken once for work it can spend a while on rather than once per segment.
const CHUNK: usize = 16 * 1024;

/// How many chunks may be in flight in either direction before the socket that is filling
/// the channel has to wait. Small on purpose: the buffers that matter are the guest's TCP
/// windows, and a deep channel here would only hide a slow peer from them.
const DEPTH: usize = 8;

/// What a host socket has to say to the loop.
pub(crate) enum FromHost {
    /// Bytes off a stream, or one datagram.
    Data(Vec<u8>),

    /// The peer is done sending. For TCP this becomes a FIN to the guest; for UDP it is
    /// the socket having gone away, which ends the flow.
    Eof,

    /// There is no connection and there is not going to be one. The guest gets a reset,
    /// which is the same thing it would have got from the host directly.
    Failed,
}

/// A flow's host end, as the loop holds it.
pub(crate) struct Host {
    /// Bytes for the peer, until [`finish`](Host::finish) takes it away. Dropping it is
    /// the only way to say the guest has stopped sending: the task is waiting on this
    /// channel, and a channel with no sender is what it turns into a shutdown.
    out: Option<mpsc::Sender<Vec<u8>>>,

    /// What the peer sent, and how the flow ends.
    pub(crate) incoming: mpsc::Receiver<FromHost>,
}

impl Host {
    /// Room for one more chunk, or `None` when there is none -- which is a peer that has
    /// stopped reading, and the loop's signal to stop taking bytes out of the guest's
    /// window.
    pub(crate) fn reserve(&self) -> Option<mpsc::Permit<'_, Vec<u8>>> {
        self.out.as_ref()?.try_reserve().ok()
    }

    /// One chunk, dropped if there is no room. What that means depends on the protocol,
    /// and for the one that uses it -- UDP -- a dropped datagram is within the contract.
    pub(crate) fn send(&self, payload: Vec<u8>) {
        if let Some(out) = &self.out {
            let _ = out.try_send(payload);
        }
    }

    /// The guest has nothing more to send. The peer's socket is shut for writing once the
    /// task notices, which is as soon as it has drained what is already in the channel.
    pub(crate) fn finish(&mut self) {
        self.out = None;
    }
}

/// Connect to `target` and pump, reporting to the loop through the returned [`Host`].
///
/// The connect happens in the task, so this returns before it completes -- a flow is
/// open on the guest's side from the moment its SYN arrived, and what a refused
/// connection turns into is [`FromHost::Failed`] a moment later rather than a wait here.
pub(crate) fn tcp(target: SocketAddrV4, wake: Arc<Notify>) -> Host {
    let (out, mut outbound) = mpsc::channel::<Vec<u8>>(DEPTH);
    let (inbound, incoming) = mpsc::channel::<FromHost>(DEPTH);

    tokio::spawn(async move {
        let stream = match TcpStream::connect(target).await {
            Ok(stream) => stream,
            Err(_) => {
                report(&inbound, &wake, FromHost::Failed).await;
                return;
            }
        };
        // The guest's segments are already whatever size it chose to send; holding them
        // back for a peer that may have nothing more to say only adds a round trip to
        // every request-shaped exchange, which is most of them.
        let _ = stream.set_nodelay(true);

        let (mut from_peer, mut to_peer) = stream.into_split();

        let reader = tokio::spawn({
            let (inbound, wake) = (inbound.clone(), wake.clone());
            async move {
                let mut buf = vec![0u8; CHUNK];
                loop {
                    let read = match from_peer.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(read) => read,
                    };
                    if !report(&inbound, &wake, FromHost::Data(buf[..read].to_vec())).await {
                        return;
                    }
                }
                report(&inbound, &wake, FromHost::Eof).await;
            }
        });

        // Ends when the loop drops its sender, which is the guest having closed its half.
        while let Some(chunk) = outbound.recv().await {
            if to_peer.write_all(&chunk).await.is_err() {
                break;
            }
        }
        let _ = to_peer.shutdown().await;
        let _ = reader.await;
    });

    Host {
        out: Some(out),
        incoming,
    }
}

/// The same for a datagram flow: one host socket per guest port and destination, so that
/// what comes back is only ever what this flow's own peer sent.
///
/// Connected rather than bound-and-addressed, which is what makes that true: the kernel
/// then refuses datagrams from anywhere but `target`, and an off-path answer never
/// reaches the guest.
pub(crate) fn udp(target: SocketAddrV4, wake: Arc<Notify>) -> Host {
    let (out, mut outbound) = mpsc::channel::<Vec<u8>>(DEPTH);
    let (inbound, incoming) = mpsc::channel::<FromHost>(DEPTH);

    tokio::spawn(async move {
        let socket = match UdpSocket::bind(("0.0.0.0", 0)).await {
            Ok(socket) => socket,
            Err(_) => {
                report(&inbound, &wake, FromHost::Failed).await;
                return;
            }
        };
        if socket.connect(target).await.is_err() {
            report(&inbound, &wake, FromHost::Failed).await;
            return;
        }

        let socket = Arc::new(socket);
        let reader = tokio::spawn({
            let (socket, inbound, wake) = (socket.clone(), inbound.clone(), wake.clone());
            async move {
                // One datagram per read, and a buffer that cannot truncate one the guest
                // could have sent -- a short read would silently deliver half an answer.
                let mut buf = vec![0u8; u16::MAX as usize];
                loop {
                    let Ok(read) = socket.recv(&mut buf).await else {
                        break;
                    };
                    if !report(&inbound, &wake, FromHost::Data(buf[..read].to_vec())).await {
                        return;
                    }
                }
                report(&inbound, &wake, FromHost::Eof).await;
            }
        });

        while let Some(datagram) = outbound.recv().await {
            if socket.send(&datagram).await.is_err() {
                break;
            }
        }
        reader.abort();
    });

    Host {
        out: Some(out),
        incoming,
    }
}

/// Hand one thing to the loop and wake it. `false` once the loop has dropped the flow,
/// which is a task's signal to stop.
async fn report(inbound: &mpsc::Sender<FromHost>, wake: &Notify, what: FromHost) -> bool {
    let sent = inbound.send(what).await.is_ok();
    wake.notify_one();
    sent
}

/// The largest payload this stack will put in one frame to the guest, which a datagram
/// bigger than has to be dropped rather than fragmented -- see the stack's own note.
pub(crate) const MAX_DATAGRAM: usize = MTU - 20 - 8;
