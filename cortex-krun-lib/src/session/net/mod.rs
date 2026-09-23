//! The network a session gets, and the stack on the host that is the whole of it.
//!
//! # Why a stack and not a switch
//!
//! Because there is nothing to switch onto. libkrun's virtio-net device does not bridge,
//! route or NAT: it takes one socket and puts every ethernet frame the guest sends down
//! it, and puts back whatever it reads. Everything a network *is* -- an address for the
//! guest, something to answer its ARP, a gateway that forwards, a resolver -- is on the
//! other end of that socket, and this module is that other end.
//!
//! ```text
//!      guest                    │ hypervisor │            host
//!   ───────────────────────────────────────────────────────────────────
//!   eth0 ──► virtio-net queue ──┼─ socketpair ─┼──► Stack ──► TcpStream
//!                               │            │      │        UdpSocket
//!                               │            │      ├─ ARP, DHCP, DNS
//!                               │            │      └─ Policy
//! ```
//!
//! The socket is a `socketpair`, so no path is created, nothing else can connect to it,
//! and the whole network dies with the process holding it. One datagram is one ethernet
//! frame -- see [`device`].
//!
//! # What the guest does not get
//!
//! An interface it has to configure, for one: libkrun's init runs a DHCP client when
//! [`DHCP_ENV`] is set, and this stack is the server that answers it. That is also what
//! writes `/etc/resolv.conf` in the guest, which is the difference between a network that
//! works and one that only works by address.
//!
//! No IPv6. The guest may ask; nothing here answers, so it falls back to v4 rather than
//! hanging. No inbound connections: a host reaching *into* a session is a grant with its
//! own name and this is not it. And no raw sockets -- a `ping` from the guest is answered
//! by this stack itself or not at all, because what is behind it is host sockets, and an
//! unprivileged host process cannot send an ICMP echo.
//!
//! # Where the policy is
//!
//! In [`policy`], and applied at one place: the moment a flow asks for a host socket. It
//! is not a firewall in front of a network -- it *is* the network, and a destination the
//! policy refuses is a destination this stack never opens a socket to. There is nothing
//! in the guest that can route around it, because the only way out of the guest is the
//! queue this reads.

mod device;
mod dhcp;
mod policy;
mod proxy;
mod resolver;
mod stack;

use std::net::Ipv4Addr;

use super::{Error, Result};

pub(crate) use policy::Policy;
pub(crate) use stack::Stack;

/// How much of the network a session may reach.
///
/// The names are the same four `cortex-krun-host` spells on its wire, and mean the same
/// four things -- a session's reach is decided by whoever starts it, and this is what
/// that decision turns into.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Network {
    /// No device at all, which is the strongest statement available: there is nothing to
    /// configure, nothing to route over and nothing to refuse, because the guest has no
    /// interface but `lo`.
    ///
    /// The default. A session that did not ask to reach anything does not.
    #[default]
    Disabled,

    /// A device, a resolver, and whatever [`host_ports`](Policy::host_ports) granted.
    /// Nothing else: every destination outside the guest's own subnet is refused.
    ///
    /// What makes it a reach rather than an elaborate way of saying `Disabled` is the
    /// grant -- a session that was given a port talks to whatever the caller put on this
    /// machine, with none of it being on the internet.
    ///
    /// **Not an air gap.** The resolver forwards to the host's, so a name is a way to
    /// spell something to the outside even when no packet of the guest's gets there.
    /// [`Disabled`](Self::Disabled) is the air gap.
    HostOnly,

    /// The public internet, and the grant. Private ranges, link-local addresses, carrier
    /// NAT space and this machine's own loopback stay refused -- a sandbox that can reach
    /// the LAN is reaching things nobody published to it.
    Public,

    /// Every address, with no judgement but the grant's. For a caller who has decided the
    /// sandbox boundary is somewhere else.
    ///
    /// Still not the host: reaching this machine is [`host_ports`](Policy::host_ports)
    /// and only that, at every level -- see the module doc.
    Full,
}

impl Network {
    /// The name this level is spelled with, which is the one `--network` takes.
    pub fn as_str(self) -> &'static str {
        match self {
            Network::Disabled => "none",
            Network::HostOnly => "host",
            Network::Public => "public",
            Network::Full => "full",
        }
    }

    /// The level a name spells, and `None` for a word that is not one of the four.
    ///
    /// The inverse of [`as_str`](Self::as_str), and here beside it because a spelling with
    /// only one half written down is one that drifts: a caller reading `--network` and a
    /// caller writing it are in different processes, and what they disagree about is how much
    /// of the network a sandbox gets.
    ///
    /// An unrecognised word is `None` rather than a level, and never
    /// [`Disabled`](Self::Disabled): `on`, `true` and `1` are all things somebody might type
    /// and none of them says how much reach is wanted, so the answer belongs to whoever can
    /// say which spelling was rejected.
    pub fn parse(name: &str) -> Option<Network> {
        match name {
            "none" => Some(Network::Disabled),
            "host" => Some(Network::HostOnly),
            "public" => Some(Network::Public),
            "full" => Some(Network::Full),
            _ => None,
        }
    }

    /// Whether a session at this level gets a device -- and so whether there is a stack
    /// to start at all.
    pub(crate) fn attached(self) -> bool {
        self != Network::Disabled
    }
}

/// Told to libkrun's init as `KRUN_DHCP=1`, which is what makes it run a DHCP client on
/// `eth0` -- and, with it, write the `/etc/resolv.conf` this stack's answer names.
pub(crate) const DHCP_ENV: &str = "KRUN_DHCP";

/// The guest's subnet, which nothing but the guest and this stack is on.
///
/// 10.0.2.0/24 because it is what every user-mode network since slirp has used, so an
/// address seen in a log reads as "the sandbox" to anyone who has looked at one before.
/// Nothing routes it: the host reaches these addresses through no interface at all, and
/// the only thing that resolves them is this module.
pub(crate) const SUBNET_PREFIX: u8 = 24;

/// This stack, as the guest addresses it: its default route, the only address it can ARP
/// for, and the resolver its `/etc/resolv.conf` is given.
///
/// One address for all three because they are one thing -- a gateway that forwards is
/// already the only host the guest can reach, so giving the resolver a second address
/// would be inventing a second machine to put it on.
pub(crate) const GATEWAY: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 1);

/// What the guest is leased. A single address rather than a pool: there is one guest, it
/// is the only thing on this subnet, and a lease it can lose is a session that loses its
/// network while it runs.
pub(crate) const GUEST: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 15);

/// The gateway's hardware address, which is this stack's own.
///
/// Locally administered (the `0x02` bit) and not a real card's, because it is not a card:
/// it exists so that the guest's ARP has something to put in a frame's destination.
pub(crate) const GATEWAY_MAC: [u8; 6] = [0x5a, 0x94, 0xef, 0xe4, 0x0c, 0x01];

/// The guest's hardware address, which the host chooses because the host makes the
/// device. Fixed rather than random, so that two runs of one session are one machine as
/// far as anything that remembers a MAC is concerned.
pub(crate) const GUEST_MAC: [u8; 6] = [0x5a, 0x94, 0xef, 0xe4, 0x0c, 0x15];

/// The largest ethernet frame either end will send. The guest is told this in its DHCP
/// lease, so both ends agree without either fragmenting.
pub(crate) const MTU: usize = 1500;
