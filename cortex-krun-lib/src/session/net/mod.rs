//! The network a session gets: the guest's own sockets, opened on the host.
//!
//! # Why sockets and not a device
//!
//! Because a session's network is either all of what this host reaches or none of it, and
//! all of what this host reaches is exactly what its sockets are. libkrun's Transparent
//! Socket Impersonation gives the guest kernel an address family that speaks AF_INET and
//! AF_INET6 to the program and vsock to the host: a `connect`, a `sendto` or a `listen` in
//! the guest arrives here as a request, and libkrun opens the host socket that answers it.
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
//! configure and nothing on the host to forward. What is left to decide is the handful of
//! places where "open what the guest asked for" is not the answer -- and those are
//! [`policy`].
//!
//! # What a guest's own sockets still are
//!
//! The guest's. The kernel tries its own stack first and falls back to the host only for
//! what its own stack cannot reach, so two programs in the guest talking over its loopback
//! never leave it, and a `listen` the host is not asked to take stays a listener in the
//! guest.
//!
//! # A session with none
//!
//! Gets no device, and so no way off its own loopback -- see [`Session::network`]. There is
//! nothing in the guest that can put one back.
//!
//! [`Session::network`]: super::Session::network

mod policy;
mod relay;
mod resolver;

use std::net::{Ipv4Addr, TcpListener};
use std::sync::Arc;

use krun::{TsiFlags, VsockDevice};

use policy::Policy;

use super::{Error, Result};

/// Start this session's network and hand back the device that is its near end.
///
/// `published` are the listeners on this host whose connections reach a console port in the
/// guest -- each one bound by the caller, which is the end that knows where it is and says
/// so. Each gets a thread that carries its connections to wherever the guest is listening on
/// that port by then.
pub(crate) fn attach(published: Vec<(u16, TcpListener)>) -> Result<VsockDevice> {
    let policy = Arc::new(
        Policy::new(
            resolver::host_resolvers().first().copied(),
            published.iter().map(|(console, _)| *console),
        )
        .with_relays(
            published
                .iter()
                .filter_map(|(console, listener)| Some((*console, listener.local_addr().ok()?))),
        ),
    );
    for (console, listener) in published {
        relay::spawn(console, listener, policy.clone())
            .map_err(|e| Error::io(format!("relaying into guest port {console}"), e))?;
    }

    let mut device = VsockDevice::new(GUEST_CID, TsiFlags::HIJACK_INET)
        .map_err(|e| Error::vmm("attaching the guest's network", e))?;
    device.set_policy(policy);
    Ok(device)
}

/// Told to libkrun's init as `KRUN_NAMESERVER`, which is what it writes into the guest's
/// `/etc/resolv.conf` -- there being no lease to carry one.
pub(crate) const NAMESERVER_ENV: &str = "KRUN_NAMESERVER";

/// The resolver the guest is told it has, and which [`policy`] carries to this host's.
///
/// An address rather than the host's own resolver, because the host's is often one the
/// guest could not name -- `127.0.0.53` behind systemd, a VPN's, one that changes when a
/// laptop does. 10.0.2.3 because it is the one every user-mode network since slirp has put
/// its resolver at, so it reads as "the sandbox's" to anyone who has looked at one. Nothing
/// routes it: the guest has no stack that reaches it, which is what hands a query to the
/// host, and the policy is the only thing that knows what it means.
pub(crate) const NAMESERVER: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 3);

/// The guest's vsock address. Any number above the reserved two would do: there is one
/// guest on this device and nothing addresses it by number.
const GUEST_CID: u64 = 3;
