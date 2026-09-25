//! The wire on a host with no `socketpair`: a connected pair on loopback, and a length in
//! front of every frame.
//!
//! **Run once, on one machine.** Written from libkrun's own Windows backend rather than
//! against it, and first exercised on an x86_64 Windows host: a guest takes its address
//! over DHCP and reaches the public internet. What that first run turned up was not here
//! but in libkrun -- its Windows builder closed the socket it had been given -- and is
//! carried as `libkrun-85bed71-windows-net.patch`. Nothing beyond one host and one guest
//! has been tried, so treat what follows as described rather than as proven.
//!
//! # Why a pair on loopback and not AF_UNIX
//!
//! Windows does have AF_UNIX, and libkrun's backend connects to one -- but only stream
//! sockets, only by a path on the filesystem, and neither `std` nor `tokio` will make a
//! *listener* on one. Being the listening end would mean hand-rolling winsock and then
//! driving it from a thread of its own, because there is no way to hand tokio a socket it
//! did not make.
//!
//! What libkrun actually requires turns out to be much less than AF_UNIX. Its Windows
//! backend takes the socket already connected and does nothing to it but `recv`, `send`
//! and two buffer options -- so any connected stream socket is one it can speak over, and
//! a loopback pair is one `std` can make and `tokio` can drive.
//!
//! # The window, and what closes it
//!
//! A pair on loopback is made by listening, connecting to the listener, and accepting --
//! and between the listen and the accept, another process on this machine could connect
//! first. The port is ephemeral and the listener lives for the length of one `connect`,
//! but "unlikely" is not "cannot", so the accepted peer's address is checked against the
//! connecting end's before either is used. A connection from anywhere else is refused and
//! the whole pair is abandoned.
//!
//! This is the one thing the unix wire gets for free and this one has to buy, and it is
//! the reason to prefer a real AF_UNIX pair here if the blocking-thread cost is ever
//! worth paying.
//!
//! # The framing
//!
//! Four bytes of big-endian length, then that many bytes of frame. libkrun's, not this
//! module's: it is what passt speaks and what the code on the other end of this socket is
//! already written to. A stream has no boundaries of its own, so without it there would
//! be no way to tell where one frame stopped.

use std::collections::VecDeque;
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::os::windows::io::{FromRawHandle, IntoRawSocket, OwnedHandle, RawHandle};

use super::MAX_FRAME;
use crate::session::net::{Error, GUEST_MAC, Result};

/// The length in front of a frame, which is what makes a stream into frames at all.
const HEADER: usize = 4;

/// How much may be waiting to go to the guest before frames are dropped instead of
/// queued. A guest that has stopped reading its queue is one whose network is already
/// not working; growing a buffer for it would turn that into this process running out of
/// memory.
const OUTBOX_CAP: usize = 4 << 20;

/// The stack's end before there is a runtime to register it on.
pub(crate) struct Pending(TcpStream);

/// One connected pair, and the device on the other end of it.
pub(crate) fn pair() -> Result<(Pending, krun::NetDevice)> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .map_err(|e| Error::io("listening for the guest's network wire", e))?;
    let at = listener
        .local_addr()
        .map_err(|e| Error::io("naming the guest's network wire", e))?;

    let guest =
        TcpStream::connect(at).map_err(|e| Error::io("connecting the guest's network wire", e))?;
    let ours = guest
        .local_addr()
        .map_err(|e| Error::io("naming the guest's network wire", e))?;

    let (host, peer) = listener
        .accept()
        .map_err(|e| Error::io("accepting the guest's network wire", e))?;

    // Whoever got there first was supposed to be us -- see the module doc. Nothing else
    // may be on either end of a wire that carries a session's whole network.
    if peer != ours {
        return Err(Error::io(
            format!("the guest's network wire was connected by {peer} and not by this process"),
            std::io::Error::from(std::io::ErrorKind::ConnectionRefused),
        ));
    }
    drop(listener);

    // Frames are small and a session's traffic is mostly request-shaped, so waiting to
    // coalesce them would add a delay to every exchange and save nothing.
    for end in [&guest, &host] {
        let _ = end.set_nodelay(true);
    }
    host.set_nonblocking(true)
        .map_err(|e| Error::io("putting the network socket in non-blocking mode", e))?;

    // libkrun takes a handle and casts it straight back to a socket, which is what a
    // socket already is on this platform. Ownership goes with it: nothing here closes
    // this end again.
    let raw = guest.into_raw_socket();
    // SAFETY: `into_raw_socket` gave up ownership of a live socket, and a `SOCKET` is a
    // kernel handle -- which is the same cast libkrun makes on the other side of this
    // call to get it back.
    let handle = unsafe { OwnedHandle::from_raw_handle(raw as RawHandle) };

    let device = krun::NetDevice::new_unixstream_handle(
        "net0",
        handle,
        &GUEST_MAC,
        // No offloads and no extra queues -- see the unix wire, which says the same.
        0,
        krun::NetFlags::empty(),
    )
    .map_err(|e| Error::vmm("attaching the guest's network device", e))?;

    Ok((Pending(host), device))
}

pub(crate) struct Wire {
    stream: tokio::net::TcpStream,

    /// Bytes read and not yet a whole frame. A stream splits frames wherever it likes, so
    /// what is here between calls is the part of one that has not arrived yet.
    inbox: Vec<u8>,

    /// Frames written and not yet taken by the socket, each with its length in front.
    outbox: Vec<u8>,

    /// How far into `outbox` the socket has got. An offset rather than draining from the
    /// front, because a partial write is the common case and moving the remainder down on
    /// every one of them is work for nothing.
    sent: usize,
}

impl Wire {
    /// Registers the socket with the reactor, so it has to be called on the runtime that
    /// is going to drive it.
    pub(crate) fn new(pending: Pending) -> Wire {
        Wire {
            stream: tokio::net::TcpStream::from_std(pending.0)
                .expect("a non-blocking socket, on the runtime that will drive it"),
            inbox: Vec::with_capacity(MAX_FRAME * 4),
            outbox: Vec::new(),
            sent: 0,
        }
    }

    /// Every whole frame that has arrived since the last call. A partial one stays in
    /// [`inbox`](Wire::inbox) until the rest of it turns up.
    pub(crate) fn drain(&mut self, frames: &mut VecDeque<Vec<u8>>) {
        let mut chunk = [0u8; MAX_FRAME];
        while let Ok(read) = self.stream.try_read(&mut chunk) {
            if read == 0 {
                break;
            }
            self.inbox.extend_from_slice(&chunk[..read]);
        }

        let mut at = 0;
        while self.inbox.len() - at >= HEADER {
            let len = u32::from_be_bytes(
                self.inbox[at..at + HEADER]
                    .try_into()
                    .expect("four bytes, taken as four"),
            ) as usize;

            // A length nothing could have sent means the two ends have lost each other in
            // the stream, and every byte after it would be read at the wrong offset.
            // There is no resynchronising a stream, so what is left is dropped.
            if len > MAX_FRAME {
                self.inbox.clear();
                return;
            }
            if self.inbox.len() - at < HEADER + len {
                break;
            }

            frames.push_back(self.inbox[at + HEADER..at + HEADER + len].to_vec());
            at += HEADER + len;
        }
        self.inbox.drain(..at);
    }

    /// One frame, with its length in front, dropped if the guest has stopped reading --
    /// see [`OUTBOX_CAP`].
    pub(crate) fn send(&mut self, frame: &[u8]) {
        if self.outbox.len() - self.sent + HEADER + frame.len() > OUTBOX_CAP {
            return;
        }
        self.outbox
            .extend_from_slice(&(frame.len() as u32).to_be_bytes());
        self.outbox.extend_from_slice(frame);
    }

    /// As much of what is queued as the socket will take.
    pub(crate) fn flush(&mut self) {
        while self.sent < self.outbox.len() {
            match self.stream.try_write(&self.outbox[self.sent..]) {
                Ok(0) | Err(_) => break,
                Ok(written) => self.sent += written,
            }
        }
        if self.sent == self.outbox.len() {
            self.outbox.clear();
            self.sent = 0;
        }
    }

    /// Until there is a frame to read -- or, when the socket owes the guest something,
    /// until it will take more of it.
    pub(crate) async fn ready(&self) {
        if self.sent < self.outbox.len() {
            tokio::select! {
                _ = self.stream.readable() => {}
                _ = self.stream.writable() => {}
            }
        } else {
            let _ = self.stream.readable().await;
        }
    }
}
