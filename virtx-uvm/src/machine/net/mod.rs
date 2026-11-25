//! The network a machine gets: a virtio-net device, and a TCP/IP stack in this process at the
//! other end of its link.
//!
//! ```text
//!      guest                    │ hypervisor │                this process
//!   ──────────────────────────────────────────────────────────────────────────────────
//!   eth0 ──► virtio-net ────────┼── libkrun ─┼──► Link ──► Stack ──► Flow ──► host socket
//!   10.0.2.15/24, via 10.0.2.2  │            │   (frames)    │
//!                               │            │             Policy
//! ```
//!
//! Read in that order: [`device`] makes the device and starts the [`Stack`](stack::Stack) on a
//! thread of its own; the stack turns frames from the [`Link`](link::Link) into
//! [`Flow`](flow::Flow)s -- a [TCP connection](tcp::TcpFlow) or a [UDP flow](udp::UdpFlow), each
//! carried on a host socket -- to where the [`Policy`](policy::Policy) says.
//!
//! The guest has an interface of its own, which the agent sets up ([`GuestNetwork`]), and its
//! own kernel's stack: its loopback, its listeners and its ports never leave it.
//!
//! # The way out and the way in
//!
//! Apart, as the protocol has them. A machine with a network reaches what the internet routes,
//! and resolves names through this host's resolver; one without reaches nothing. A published
//! port is a listener on this host whose connections the stack opens to that port at the
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

mod flow;
mod link;
mod policy;
mod resolver;
mod stack;
mod tcp;
mod udp;

use std::net::{Ipv4Addr, TcpListener};

use krun::{NetDevice, NetFlags};

pub(crate) use policy::routed;

use super::wire::GuestNetwork;
use super::{Error, Result};
use link::DeviceEnd;
use policy::Policy;
use stack::Stack;

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
            mtu: LAN.mtu,
            nameserver: egress.then_some(LAN.nameserver),
        }
    }
}

/// The guest's virtio-net device, its stack started at the other end: out to what the internet
/// routes if `egress`, and in through `published`, the listeners on this host whose
/// connections reach a port in the guest -- each bound by the caller, which is the end that
/// knows where it is and says so.
pub(crate) fn device(egress: bool, published: Vec<(u16, TcpListener)>) -> Result<NetDevice> {
    // `VIRTIO_NET_F_CSUM`, `VIRTIO_NET_F_GUEST_CSUM` and `VIRTIO_NET_F_GUEST_TSO4`: see the
    // module doc.
    const FEATURES: u32 = (1 << 0) | (1 << 1) | (1 << 7);

    let (end, pending) = link::open().map_err(|e| Error::io("opening the guest's link", e))?;
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

    let policy = Policy::new(egress);
    std::thread::Builder::new()
        .name("net".into())
        .spawn(move || {
            let run = || Stack::new(pending.connect()?, policy, published)?.run();
            if let Err(e) = run() {
                log::error!("the guest's network stopped: {e}");
            }
        })
        .map_err(|e| Error::io("starting the guest's network", e))?;
    Ok(device)
}
