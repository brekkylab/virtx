//! The wire on a host that has `socketpair`: one datagram, one frame, no framing.
//!
//! # Why a socketpair and not a socket file
//!
//! libkrun will take either -- [`new_unixgram_path`](krun::NetDevice::new_unixgram_path)
//! opens a path and [`new_unixgram_fd`](krun::NetDevice::new_unixgram_fd) takes a
//! descriptor. A path is what a session wants when the stack is another process, which is
//! how vfkit and passt are used; here the stack is a thread in this one, and a path would
//! mean a name in the filesystem that anything on the host could connect to, a name to
//! clean up, and a race between making it and the guest coming up. A pair has none of
//! those: it is already connected, it is reachable by nothing but the two descriptors,
//! and it is gone when they are closed.
//!
//! The vfkit magic is not sent either. libkrun writes it only when it opens a path, and a
//! pair has no one to introduce itself to.
//!
//! # The buffers, which have to be said out loud
//!
//! libkrun sizes its socket's buffers only on the path it opens itself, so a pair gets
//! whatever the kernel's default is -- and on macOS the *send* buffer of a unix datagram
//! socket is not a queue but a ceiling on one datagram, which the default sets low enough
//! to refuse an ordinary frame. Both ends are sized here, so neither can be the end that
//! silently drops.

use std::collections::VecDeque;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::UnixDatagram;

use super::MAX_FRAME;
use crate::session::net::{Error, GUEST_MAC, Result};

/// What each end of the pair may hold. Generous, and deliberately so: the cost is kernel
/// memory that is only committed as it is used, and what it buys is that a burst -- a
/// download at line rate, a package index arriving -- is queued rather than dropped on
/// the floor by whichever end got to the scheduler second.
const SOCKET_BUF: libc::c_int = 4 << 20;

/// The stack's end before there is a runtime to register it on.
pub(crate) struct Pending(UnixDatagram);

/// One `socketpair`, and the device on the other end of it.
pub(crate) fn pair() -> Result<(Pending, krun::NetDevice)> {
    let (guest, host) = UnixDatagram::pair()
        .map_err(|e| Error::io("making the socketpair the guest's network runs over", e))?;

    for end in [&guest, &host] {
        size(end).map_err(|e| Error::io("sizing the network socket's buffers", e))?;
    }
    host.set_nonblocking(true)
        .map_err(|e| Error::io("putting the network socket in non-blocking mode", e))?;

    let device = krun::NetDevice::new_unixgram_fd(
        "net0",
        OwnedFd::from(guest),
        &GUEST_MAC,
        // No offloads and no extra queues: everything on the other side of this device is
        // one thread doing one frame at a time, and a feature it does not implement is one
        // the guest would be told it had.
        0,
        krun::NetFlags::empty(),
    )
    .map_err(|e| Error::vmm("attaching the guest's network device", e))?;

    Ok((Pending(host), device))
}

/// Both buffers on one end -- see the module doc on why this is not left to the default.
fn size(socket: &UnixDatagram) -> io::Result<()> {
    let size = SOCKET_BUF;
    for option in [libc::SO_SNDBUF, libc::SO_RCVBUF] {
        // SAFETY: a valid descriptor, an option both ends of the pair accept, and a
        // pointer to a `c_int` described with its own size.
        let set = unsafe {
            libc::setsockopt(
                socket.as_raw_fd(),
                libc::SOL_SOCKET,
                option,
                (&raw const size).cast(),
                size_of_val(&size) as libc::socklen_t,
            )
        };
        if set < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

pub(crate) struct Wire {
    socket: tokio::net::UnixDatagram,

    /// Read into and handed on, never kept: a datagram arrives whole or not at all, so
    /// there is nothing to carry between calls.
    buf: Vec<u8>,
}

impl Wire {
    /// Registers the socket with the reactor, so it has to be called on the runtime that
    /// is going to drive it.
    pub(crate) fn new(pending: Pending) -> Wire {
        Wire {
            socket: tokio::net::UnixDatagram::from_std(pending.0)
                .expect("a non-blocking socket, on the runtime that will drive it"),
            buf: vec![0u8; MAX_FRAME],
        }
    }

    /// Every frame that has arrived since the last call.
    pub(crate) fn drain(&mut self, frames: &mut VecDeque<Vec<u8>>) {
        while let Ok(read) = self.socket.try_recv(&mut self.buf) {
            frames.push_back(self.buf[..read].to_vec());
        }
    }

    /// One frame, dropped if the socket will not take it -- see the module doc.
    pub(crate) fn send(&mut self, frame: &[u8]) {
        let _ = self.socket.try_send(frame);
    }

    /// Nothing: a datagram is written whole by [`send`](Wire::send) or not at all.
    pub(crate) fn flush(&mut self) {}

    /// Until there is a frame to read.
    pub(crate) async fn ready(&self) {
        let _ = self.socket.readable().await;
    }
}
