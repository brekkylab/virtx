//! The wire between the device and the stack: one `socketpair`, one frame per datagram.
//!
//! # Why a socketpair and not a socket file
//!
//! libkrun will take either -- [`NetDevice::new_unixgram_path`] opens a path and
//! [`new_unixgram_fd`](krun::NetDevice::new_unixgram_fd) takes a descriptor. A path is
//! what a session wants when the stack is another process, which is how vfkit and passt
//! are used; here the stack is a thread in this one, and a path would mean a name in the
//! filesystem that anything on the host could connect to, a name to clean up, and a race
//! between making it and the guest coming up. A socketpair has none of those: it is
//! already connected, it is reachable by nothing but the two descriptors, and it is gone
//! when they are closed.
//!
//! # The framing, which is not a framing
//!
//! A datagram *is* an ethernet frame -- no length prefix, no header, no handshake. That
//! is the whole of the protocol, and it is the one libkrun's `Unixgram` backend speaks:
//! it `recv`s into a buffer and sends what it read on as a frame, and `send`s a frame
//! per call. The `virtio_net_hdr` the guest's queue carries is added and stripped on
//! libkrun's side and never reaches this socket.
//!
//! The vfkit magic is not sent either. libkrun writes it only when it opens a path, and
//! a pair has no one to introduce itself to.
//!
//! # The buffers, which have to be said out loud
//!
//! libkrun sizes its socket's buffers only on the path it opens itself, so a pair gets
//! whatever the kernel's default is -- and on macOS the *send* buffer of a unix datagram
//! socket is not a queue but a ceiling on one datagram, which the default sets low enough
//! to refuse an ordinary frame. Both ends are sized here, so neither can be the end that
//! silently drops.

use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::UnixDatagram;

use super::{Error, MTU, Result};

/// Room for a frame and the headers around it, with the slack that makes a partial read
/// impossible rather than merely unlikely. Datagrams are all-or-nothing, so a buffer that
/// is too small does not truncate a frame -- it loses one.
pub(crate) const FRAME_BUF: usize = MTU + 128;

/// What each end of the pair may hold. Generous, and deliberately so: the cost is
/// kernel memory that is only committed as it is used, and what it buys is that a burst
/// -- a `wget` at line rate, a package index arriving -- is queued rather than dropped
/// on the floor by whichever end got to the scheduler second.
const SOCKET_BUF: libc::c_int = 4 << 20;

/// The two ends of one wire. [`krun`](Pair::krun) is handed to the device and never
/// touched again; [`host`](Pair::host) is the stack's.
pub(crate) struct Pair {
    /// Given to [`krun::NetDevice::new_unixgram_fd`], which takes ownership of it.
    pub(crate) krun: OwnedFd,

    /// The stack's end, still blocking. [`Stack`](super::Stack) is what makes it
    /// non-blocking, because that is a property of how it is going to be driven rather
    /// than of the wire.
    pub(crate) host: UnixDatagram,
}

impl Pair {
    pub(crate) fn new() -> Result<Pair> {
        let (krun, host) = UnixDatagram::pair()
            .map_err(|e| Error::io("making the socketpair the guest's network runs over", e))?;

        for end in [&krun, &host] {
            size(end).map_err(|e| Error::io("sizing the network socket's buffers", e))?;
        }

        Ok(Pair {
            krun: OwnedFd::from(krun),
            host,
        })
    }
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
