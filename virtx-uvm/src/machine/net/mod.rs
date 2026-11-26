//! The network a machine gets: a virtio-net device, and a TCP/IP stack in this process at the
//! other end of its link.
//!
//! ```text
//!      guest                    │ hypervisor │                this process
//!   ──────────────────────────────────────────────────────────────────────────────────
//!   eth0 ──► virtio-net ────────┼── libkrun ─┼──► Link ──► Gateway ──► Flow ──► host socket
//!   10.0.2.15/24, via 10.0.2.2  │            │   (frames)    │
//!                               │            │               Egress
//! ```
//!
//! [`device`] makes the device and starts the [`Gateway`] on its own thread. The gateway
//! carries traffic between the [`Link`](link::Link) and host sockets.
//!
//! The guest has an interface of its own, which the agent sets up ([`GuestNetwork`]), and its
//! own kernel's stack: its loopback, its listeners and its ports never leave it.
//!
//! # The way out and the way in
//!
//! Apart, as the protocol has them. A machine with a network reaches what the internet routes,
//! and resolves names through this host's resolver; one without reaches nothing. A published
//! port is a listener on this host whose connections the gateway opens to that port at the
//! guest's loopback -- so a program listening on `127.0.0.1` alone is reached -- with or
//! without a network.
//!
//! # The link
//!
//! Its MTU is as large as libkrun's device carries, so a guest's TCP sends segments of tens of
//! kilobytes rather than 1500 bytes, and each costs a frame. The device offers the guest
//! offloads it never uses as such: checksums the stack does not check, and segmentation, which
//! is what has the guest's driver post receive buffers that large frames fit.
//!
//! A machine with neither a network nor a published port gets no device.

mod gateway;
mod link;

use std::net::{Ipv4Addr, Ipv6Addr, TcpListener};

use krun::{NetDevice, NetFlags};

pub(crate) use gateway::routed;
#[cfg(windows)]
pub(crate) use link::path as link_path;

use super::wire::GuestNetwork;
use super::{Error, Result};
use gateway::Gateway;
use link::DeviceEnd;

/// The link's addresses, as both ends of it are told: where every user-mode network since
/// slirp has put them.
struct Lan {
    guest: Ipv4Addr,
    /// This end: the guest's way off the link.
    gateway: Ipv4Addr,
    /// The resolver the guest is told it has, which the policy carries to this host's -- an
    /// address rather than the host's own resolver, which is often one the guest could not
    /// name: `127.0.0.53` behind systemd, a VPN's, one that changes when a laptop does.
    nameserver: Ipv4Addr,
    prefix: u8,
    /// The same three on v6, in a unique local prefix: a source the guest prefers v4 over for
    /// a name with both, so a host without v6 is no different to it.
    guest_v6: Ipv6Addr,
    gateway_v6: Ipv6Addr,
    nameserver_v6: Ipv6Addr,
    prefix_v6: u8,
    /// The largest IP packet: the most libkrun's device carries in a frame, less the Ethernet
    /// header, rounded down as slirp's is.
    mtu: u32,
    guest_mac: [u8; 6],
    gateway_mac: [u8; 6],
}

const LAN: Lan = Lan {
    guest: Ipv4Addr::new(10, 0, 2, 15),
    gateway: Ipv4Addr::new(10, 0, 2, 2),
    nameserver: Ipv4Addr::new(10, 0, 2, 3),
    prefix: 24,
    guest_v6: Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0, 0x15),
    gateway_v6: Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0, 2),
    nameserver_v6: Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0, 3),
    prefix_v6: 64,
    mtu: 65520,
    guest_mac: [0x02, 0x76, 0x78, 0x00, 0x00, 0x0f],
    gateway_mac: [0x02, 0x76, 0x78, 0x00, 0x00, 0x02],
};

impl GuestNetwork {
    /// What the agent sets the guest's interface up as: told of the nameserver if `egress`.
    pub(crate) fn new(egress: bool) -> GuestNetwork {
        GuestNetwork {
            address: LAN.guest,
            prefix: LAN.prefix,
            gateway: LAN.gateway,
            address_v6: LAN.guest_v6,
            prefix_v6: LAN.prefix_v6,
            gateway_v6: LAN.gateway_v6,
            mtu: LAN.mtu,
            nameserver: egress.then_some(LAN.nameserver),
        }
    }
}

/// The guest's virtio-net device, its stack started at the other end: out to what the internet
/// routes if `egress`, and in through `published`, the listeners on this host whose
/// connections reach a port in the guest -- each bound by the caller, which is the end that
/// knows where it is and says so. Each flow to the internet is put to the embedder's
/// [`Outward`](crate::outward::Outward) too, with `given`.
pub(crate) fn device(
    egress: bool,
    given: serde_json::Value,
    published: Vec<(u16, TcpListener)>,
) -> Result<NetDevice> {
    // `VIRTIO_NET_F_CSUM`, `VIRTIO_NET_F_GUEST_CSUM` and `VIRTIO_NET_F_GUEST_TSO4`: see the
    // module doc.
    const FEATURES: u32 = (1 << 0) | (1 << 1) | (1 << 7);

    #[cfg(unix)]
    let opened = link::open();
    #[cfg(windows)]
    let opened = link::open(link::path(std::process::id()));
    let (end, pending) = opened.map_err(|e| Error::io("opening the guest's link", e))?;
    let device = match end {
        #[cfg(unix)]
        DeviceEnd::Fd(fd) => {
            NetDevice::new_unixstream_fd("net0", fd, &LAN.guest_mac, FEATURES, NetFlags::empty())
        }
        #[cfg(windows)]
        DeviceEnd::Path(path) => NetDevice::new_unixstream_path(
            "net0",
            &path,
            &LAN.guest_mac,
            FEATURES,
            NetFlags::empty(),
        ),
    }
    .map_err(|e| Error::vmm("attaching the guest's network", e))?;

    std::thread::Builder::new()
        .name("net".into())
        .spawn(move || {
            let run = || Gateway::new(pending.connect()?, egress, given, published)?.run();
            if let Err(e) = run() {
                log::error!("the guest's network stopped: {e}");
            }
        })
        .map_err(|e| Error::io("starting the guest's network", e))?;
    Ok(device)
}
