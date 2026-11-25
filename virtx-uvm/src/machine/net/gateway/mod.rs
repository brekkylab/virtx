//! The guest's network on this host: one thread, one poller over the link and every host socket,
//! and smoltcp between the link and the TCP connections.
//!
//! Each turn of [`Gateway::run`] waits for something to be ready, then:
//!
//! ```text
//!   read     every whole frame the device has written
//!   ingress  each frame: a SYN gets its connection opened first, so smoltcp finds a socket
//!            listening for it, or none and resets it; UDP goes past smoltcp to its flow; the
//!            rest smoltcp takes
//!   pump     every flow
//!   egress   what smoltcp has to send, then the link written
//!   reap     the flows that are over
//! ```

mod egress;
mod flow;
#[allow(clippy::module_inception)]
mod gateway;
mod resolver;

pub(crate) use egress::routed;
pub(super) use gateway::Gateway;

use super::{LAN, link};
