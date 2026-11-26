//! A UDP flow between the guest and this host, past smoltcp: what the guest sends between two
//! ends goes out on a host socket connected to where the policy said, as it lies in the frame;
//! what comes back is written straight into a frame for the guest, from the end the guest sent
//! to.

use std::io;
use std::mem::MaybeUninit;
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use smoltcp::phy::ChecksumCapabilities;
use smoltcp::wire::{
    EthernetAddress, EthernetFrame, EthernetProtocol, EthernetRepr, IpProtocol, Ipv4Packet,
    Ipv4Repr, Ipv6Packet, Ipv6Repr, UdpPacket,
};
use socket2::{Domain, Protocol, Socket, Type};

use super::super::LAN;
use super::super::link::{Link, MAX_FRAME};
use super::{Ends, Interest};

/// How long a flow nothing has crossed is kept.
pub(in crate::machine::net::gateway) const IDLE: Duration = Duration::from_secs(60);

pub(in crate::machine::net::gateway) struct UdpFlow {
    host: Socket,
    ends: Ends,
    last: Instant,
}

impl UdpFlow {
    /// A flow between `ends`, its host socket connected to `at`.
    pub(in crate::machine::net::gateway) fn open(
        ends: Ends,
        at: SocketAddr,
    ) -> io::Result<UdpFlow> {
        let host = Socket::new(Domain::for_address(at), Type::DGRAM, Some(Protocol::UDP))?;
        host.set_nonblocking(true)?;
        host.connect(&at.into())?;
        Ok(UdpFlow {
            host,
            ends,
            last: Instant::now(),
        })
    }

    pub(super) fn host(&self) -> &Socket {
        &self.host
    }

    /// Send what the guest sent. A datagram the host socket will not take now is dropped, as
    /// UDP may.
    pub(in crate::machine::net::gateway) fn send(&mut self, payload: &[u8]) {
        self.last = Instant::now();
        let _ = self.host.send(payload);
    }

    /// Carry what the host socket has to the guest, if it was reported, while the link will
    /// take more; and wait on it while the link will.
    pub(super) fn pump(&mut self, link: &mut Link, ready: bool) -> Interest {
        while ready && !link.full() {
            let mut got = None;
            link.send(MAX_FRAME, |frame| {
                let ip = if self.ends.0.is_ipv4() { 20 } else { 40 };
                let at = EthernetFrame::<&[u8]>::header_len() + ip + 8;
                // SAFETY: `recv` writes into what it is given and says how much it wrote, which
                // is all of it that is read.
                let payload =
                    unsafe { &mut *(&mut frame[at..] as *mut [u8] as *mut [MaybeUninit<u8>]) };
                got = Some(self.host.recv(payload));
                let len = *got.as_ref()?.as_ref().ok()?;
                Some(self.frame(frame, len))
            });
            match got {
                Some(Ok(_)) => self.last = Instant::now(),
                Some(Err(e)) if e.kind() == io::ErrorKind::Interrupted => {}
                // A datagram the host socket was refused, which a connected UDP socket is told
                // of: the next one may go through.
                Some(Err(e)) if e.kind() == io::ErrorKind::ConnectionRefused => {}
                _ => break,
            }
        }
        Interest {
            read: !link.full(),
            write: false,
        }
    }

    /// Whether nothing has crossed it for [`IDLE`].
    pub(super) fn over(&self) -> bool {
        self.last.elapsed() >= IDLE
    }

    /// Write the headers before the `len` bytes of payload already in place, from the end the
    /// guest sent to, and say how long the frame is.
    fn frame(&self, frame: &mut [u8], len: usize) -> usize {
        let (guest, sent_to) = self.ends;
        let at = EthernetFrame::<&[u8]>::header_len();
        let (ethertype, at) = match (sent_to.ip(), guest.ip()) {
            (IpAddr::V4(src_addr), IpAddr::V4(dst_addr)) => {
                let ip = Ipv4Repr {
                    src_addr,
                    dst_addr,
                    next_header: IpProtocol::Udp,
                    payload_len: 8 + len,
                    hop_limit: 64,
                };
                ip.emit(
                    &mut Ipv4Packet::new_unchecked(&mut frame[at..]),
                    &ChecksumCapabilities::default(),
                );
                (EthernetProtocol::Ipv4, at + ip.buffer_len())
            }
            (IpAddr::V6(src_addr), IpAddr::V6(dst_addr)) => {
                let ip = Ipv6Repr {
                    src_addr,
                    dst_addr,
                    next_header: IpProtocol::Udp,
                    payload_len: 8 + len,
                    hop_limit: 64,
                };
                ip.emit(&mut Ipv6Packet::new_unchecked(&mut frame[at..]));
                (EthernetProtocol::Ipv6, at + ip.buffer_len())
            }
            _ => unreachable!("a flow's ends are of one family, as the packet it came of"),
        };
        EthernetRepr {
            src_addr: EthernetAddress(LAN.gateway_mac),
            dst_addr: EthernetAddress(LAN.guest_mac),
            ethertype,
        }
        .emit(&mut EthernetFrame::new_unchecked(&mut frame[..]));
        let mut packet = UdpPacket::new_unchecked(&mut frame[at..at + 8 + len]);
        packet.set_src_port(sent_to.port());
        packet.set_dst_port(guest.port());
        packet.set_len((8 + len) as u16);
        packet.fill_checksum(&sent_to.ip().into(), &guest.ip().into());
        at + 8 + len
    }
}
