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
//! Apart, as the protocol has them. [`Network`] is where the guest's own connections may go;
//! a published port is how this host reaches a listener in the guest, and works whatever
//! the network is -- a machine with none is still reached on its published ports, and
//! reaches its own.
//!
//! # What a guest's own sockets still are
//!
//! The guest's. The kernel tries its own stack first and falls back to the host only for
//! what its own stack cannot reach, so two programs in the guest talking over its loopback
//! never leave it, and a `listen` the host is not asked to take stays a listener in the
//! guest. One on a port published after it is reached still: through the agent, which
//! connects to it on the guest's loopback for each connection to the port (`relay`).
//!
//! # A machine with neither
//!
//! Gets no device, and so no way off its own loopback. There is nothing in the guest that
//! can put one back.

mod policy;
mod relay;
mod resolver;

use std::net::{Ipv4Addr, SocketAddr, TcpListener};
use std::sync::Arc;

use krun::{TsiFlags, VsockDevice};
use serde::{Deserialize, Serialize};

use policy::Policy;

use super::{Error, Result};

/// Where the guest's own connections may go.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Network {
    /// Nowhere: the guest reaches its own loopback and its own published ports, and no
    /// names resolve.
    #[default]
    None,

    /// Everything this host reaches, its loopback and its LAN included -- as it is every
    /// other process's here. For a machine on its user's own computer.
    Host,

    /// Only addresses the internet routes: this host's loopback, private and link-local
    /// ranges, and cloud metadata at `169.254.169.254` are refused. For a machine on a
    /// server others reach, where what the host reaches is not the guest's to reach.
    Public,
}

/// Start this machine's network and hand back the device that is its near end.
///
/// `published` are the listeners on this host whose connections reach a port in the guest
/// -- each one bound by the caller, which is the end that knows where it is and says so.
/// Each gets a thread that carries its connections to wherever the guest is listening on
/// that port by then.
pub(crate) fn attach(
    network: Network,
    published: Vec<(u16, TcpListener)>,
) -> Result<(VsockDevice, Publisher)> {
    let resolver = match network {
        Network::None => None,
        Network::Host | Network::Public => resolver::host_resolvers().first().copied(),
    };
    let policy = Arc::new(
        Policy::new(network, resolver, published.iter().map(|(port, _)| *port)).with_relays(
            published
                .iter()
                .filter_map(|(port, listener)| Some((*port, listener.local_addr().ok()?))),
        ),
    );
    let mut device = VsockDevice::new(GUEST_CID, TsiFlags::HIJACK_INET)
        .map_err(|e| Error::vmm("attaching the guest's network", e))?;
    device.set_policy(policy.clone());
    let dial = dial(&mut device);

    for (port, listener) in published {
        relay::spawn(port, listener, policy.clone(), dial.clone()).map_err(|e| Error::Io {
            context: format!("relaying into guest port {port}"),
            source: e,
        })?;
    }
    Ok((device, Publisher { policy, dial }))
}

/// Where the agent is asked to connect to a guest port, for a program that listened before
/// its port was published (`relay`): a Unix socket of this process's, which libkrun listens on
/// and carries into the guest's vsock [`DIAL_PORT`](super::wire::DIAL_PORT). None on a host
/// whose standard library has no Unix socket to connect it with.
#[cfg(unix)]
fn dial(device: &mut VsockDevice) -> relay::Dial {
    let path = std::env::temp_dir().join(format!("virtx-uvm-dial-{}.sock", std::process::id()));
    // A process before this one under the same pid left its socket: libkrun binds the name.
    let _ = std::fs::remove_file(&path);
    match path.to_str() {
        Some(at) => {
            device.add_unix_port(super::wire::DIAL_PORT, at, true);
            relay::Dial(Some(path))
        }
        None => relay::Dial(None),
    }
}

#[cfg(not(unix))]
fn dial(_: &mut VsockDevice) -> relay::Dial {
    relay::Dial(None)
}

/// Publishes a guest port while the machine runs, as [`attach`] did those it booted with.
pub(crate) struct Publisher {
    policy: Arc<Policy>,
    dial: relay::Dial,
}

impl Publisher {
    /// Publish `port`: a listener on this host's loopback relayed to the guest's listener on it
    /// -- one a program in the guest listens on from now on, or one listening already. Answers where the listener is; again for one
    /// already published, whose listener stays where it is.
    pub(crate) fn publish(&self, port: u16) -> std::result::Result<(u16, SocketAddr), String> {
        if port == 0 {
            return Err("0 is not a port".into());
        }
        if self.policy.publishes(port) {
            return self
                .policy
                .relay(port)
                .map(|at| (port, at))
                .ok_or_else(|| format!("guest port {port} is published, but not where"));
        }
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .map_err(|e| format!("binding a relay for guest port {port}: {e}"))?;
        let at = listener
            .local_addr()
            .map_err(|e| format!("reading where guest port {port}'s relay is: {e}"))?;
        self.policy.publish(port, at);
        relay::spawn(port, listener, self.policy.clone(), self.dial.clone())
            .map_err(|e| format!("relaying into guest port {port}: {e}"))?;
        Ok((port, at))
    }
}

/// Told to libkrun's init as `KRUN_NAMESERVER`, which is what it writes into the guest's
/// `/etc/resolv.conf` -- there being no lease to carry one.
pub(crate) const NAMESERVER_ENV: &str = "KRUN_NAMESERVER";

/// The resolver the guest is told it has, and which [`policy`] carries to this host's.
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
