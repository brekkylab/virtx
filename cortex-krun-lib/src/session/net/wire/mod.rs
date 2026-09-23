//! The wire between the virtio-net device and the stack, and the one thing about a
//! session's network that is not the same on every host.
//!
//! # What a wire is, and what it is not
//!
//! It carries ethernet frames between two ends of one process, and that is the whole of
//! it. It does not know what is in a frame, it has no addresses of its own, and it drops
//! rather than waits -- which is what every ethernet has always done, and what the
//! protocols above it are already written to survive.
//!
//! Both platforms present the same three operations, so [`stack`](super::stack) is
//! written once: [`drain`](Wire::drain) takes whatever arrived, [`send`](Wire::send) and
//! [`flush`](Wire::flush) put frames back, and [`ready`](Wire::ready) is what the loop
//! sleeps on. What differs is underneath.
//!
//! # Why it differs
//!
//! libkrun's virtio-net device is built on a socket the host hands it, and the host has
//! different sockets:
//!
//! ```text
//!   unix     socketpair(AF_UNIX, SOCK_DGRAM)   one datagram is one frame
//!   windows  a connected pair on loopback      4-byte big-endian length, then the frame
//! ```
//!
//! On unix the datagram boundary *is* the frame boundary, so there is no framing to
//! speak of. Windows has no `socketpair` and no datagram unix socket, so the device's
//! backend there is a stream -- and a stream has to be told where a frame ends, which is
//! the length in front of it. That framing is libkrun's, not this module's invention:
//! it is the one passt speaks, and the code on the other side of the wire is already
//! written to it.
//!
//! # Making the device is the wire's job too
//!
//! Because the two are one decision. The end that goes to libkrun and the end that stays
//! here are made together, by the same call, and which libkrun constructor takes it is
//! decided by what kind of socket it turned out to be. Handing a descriptor back for a
//! caller to build a device from would be splitting one fact across two files that would
//! then have to agree.

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
pub(crate) use unix::{Pending, Wire, pair};
#[cfg(windows)]
pub(crate) use windows::{Pending, Wire, pair};

use super::MTU;

/// The largest frame either end will put on the wire. The guest is told the MTU in its
/// lease, so what arrives should never be larger -- this is the ceiling that says what
/// happens if it is, which is that the frame is dropped rather than half-read.
pub(crate) const MAX_FRAME: usize = MTU + 64;
