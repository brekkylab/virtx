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

mod flow;
mod tcp;
mod udp;

pub(super) use flow::{Ends, Flow, Flows, Turn};
pub(super) use tcp::TcpFlow;
pub(super) use udp::{IDLE as UDP_IDLE, UdpFlow};

use flow::Interest;
