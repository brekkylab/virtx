//! The network a machine gets: the guest's own sockets, opened on the host.
//!
//! # Why sockets and not a device
//!
//! libkrun's Transparent Socket Impersonation gives the guest kernel an address family that
//! speaks AF_INET and AF_INET6 to the program and vsock to the host: a `connect`, a `sendto`
//! or a `listen` in the guest arrives here as a request, and libkrun opens the host socket
//! that answers it.
//!
//! ```text
//!      guest                          │ hypervisor │            host
//!   ─────────────────────────────────────────────────────────────────────────
//!   connect() ──► AF_TSI ──► vsock ───┼────────────┼──► muxer ──► TcpStream
//!                                     │            │      │       UdpSocket
//!                                     │            │      └─ Policy
//! ```
//!
//! There is no interface, no address, no route and no lease: nothing in the guest to
//! configure and nothing on the host to forward. What is left to decide is where each
//! request gets a socket, and that is [`policy`].
//!
//! # The way out and the way in
//!
//! Apart, as the protocol has them. [`network`](super::MachineSpec::network) is whether the
//! guest's own connections go anywhere; a published port is how this host reaches a listener
//! in the guest, and works whether or not they do -- a machine with no network is still
//! reached on its published ports, and reaches its own.
//!
//! The way in is not TSI's. A connection to a published port is relayed ([`relay`]) to a
//! socket libkrun listens on for this host, which carries it to a vsock port in the guest, and
//! the agent there connects it to the guest's own loopback:
//!
//! ```text
//!      host                                   │ hypervisor │          guest
//!   ─────────────────────────────────────────────────────────────────────────────────
//!   published ──► relay ──► unix socket ──► muxer ──┼──► vsock ──► agent ──► 127.0.0.1:port
//! ```
//!
//! # What a guest's own sockets still are
//!
//! The guest's. The kernel tries its own stack first and falls back to the host only for
//! what its own stack cannot reach, so two programs in the guest talking over its loopback
//! never leave it. Every `listen` stays in the guest: this host never takes one.
//!
//! # A machine with neither
//!
//! Gets no device, and so no way off its own loopback. There is nothing in the guest that
//! can put one back.

mod policy;
mod relay;
mod resolver;

use std::net::{Ipv4Addr, TcpListener};
use std::path::Path;
use std::sync::Arc;

use krun::{TsiFlags, VsockDevice};

use policy::Policy;
pub(crate) use policy::routed;

use super::boot::utf8;
use super::wire::VSOCK_PORT_BASE;
use super::{Error, Result};

/// Start this machine's network and hand back the device that is its near end.
///
/// `network` is whether the guest's own connections reach the internet; `published` are the
/// listeners on this host whose connections reach a port in the guest -- each one bound by
/// the caller, which is the end that knows where it is and says so. Each gets a socket libkrun
/// listens on in `sockets`, carried to the guest port's vsock port, and a thread that relays
/// to it.
pub(crate) fn attach(
    network: bool,
    published: Vec<(u16, TcpListener)>,
    sockets: &Path,
) -> Result<VsockDevice> {
    let resolver = network
        .then(|| resolver::host_resolvers().first().copied())
        .flatten();
    let mut device = VsockDevice::new(GUEST_CID, TsiFlags::HIJACK_INET)
        .map_err(|e| Error::vmm("attaching the guest's network", e))?;
    device.set_policy(Arc::new(Policy::new(network, resolver)));

    for (port, listener) in published {
        let socket = sockets.join(format!("{port}.sock"));
        device.add_unix_port(
            VSOCK_PORT_BASE + u32::from(port),
            utf8(&socket, &format!("guest port {port}'s socket"))?,
            true,
        );
        relay::spawn(port, listener, socket).map_err(|e| Error::Io {
            context: format!("relaying into guest port {port}"),
            source: e,
        })?;
    }
    Ok(device)
}

/// The resolver the guest is told it has, which the agent writes into its `/etc/resolv.conf`
/// -- there being no lease to carry one -- and which [`policy`] carries to this host's.
///
/// An address rather than the host's own resolver, because the host's is often one the
/// guest could not name -- `127.0.0.53` behind systemd, a VPN's, one that changes when a
/// laptop does. 10.0.2.3 because it is the one every user-mode network since slirp has put
/// its resolver at. Nothing routes it: the guest has no stack that reaches it, which is what
/// hands a query to the host, and the policy is the only thing that knows what it means.
pub(crate) const NAMESERVER: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 3);

/// The guest's vsock address. Any number above the reserved two would do: there is one
/// guest on this device and nothing addresses it by number.
const GUEST_CID: u64 = 3;
