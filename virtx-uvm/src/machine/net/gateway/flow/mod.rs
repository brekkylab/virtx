//! A flow: what the guest sends between two ends, carried on a host socket of its own -- a
//! [TCP connection](tcp::TcpFlow) or a [UDP flow](udp::UdpFlow). Every flow is driven the same way, once
//! a turn:
//!
//! ```text
//!   pump    move what each side has to the other, and say what the host socket is waited on for
//!   over    whether it is done, and so put away
//! ```
//!
//! [`Flows`] opens them, as the gateway's frames and published ports ask, and keeps them, each under the key its host socket is waited on by in the poller, and
//! by its two ends. Each host socket is waited on once at a time (`PollMode::Oneshot`), and
//! asked for again only for what its flow waits on, so a socket the stack cannot keep up with is
//! not reported again until it can.

#[allow(clippy::module_inception)]
mod flow;
mod tcp;
mod udp;

pub(super) use flow::{Ends, Flows};

use flow::Interest;
