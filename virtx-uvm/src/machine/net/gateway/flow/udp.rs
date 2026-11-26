//! A UDP flow between the guest and this host, past smoltcp: what the guest sends between two
//! ends goes out on a host socket connected to where the policy said, as it lies in the frame;
//! what comes back is written straight into a frame for the guest, from the end the guest sent
//! to.

use std::io;
use std::mem::MaybeUninit;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use socket2::{Domain, Protocol, Socket, Type};

use super::super::link::Link;
use super::{Ends, Interest};

/// How long a flow nothing has crossed is kept.
pub(super) const IDLE: Duration = Duration::from_secs(60);

pub(super) struct UdpFlow {
    host: Socket,
    ends: Ends,
    last: Instant,
}

impl UdpFlow {
    /// A flow between `ends`, its host socket connected to `at`.
    pub(super) fn open(ends: Ends, at: SocketAddr) -> io::Result<UdpFlow> {
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
    pub(super) fn send(&mut self, payload: &[u8]) {
        self.last = Instant::now();
        let _ = self.host.send(payload);
    }

    /// Carry what the host socket has to the guest, if it was reported, while the link will
    /// take more; and wait on it while the link will.
    pub(super) fn pump(&mut self, link: &mut Link, ready: bool) -> Interest {
        while ready && !link.full() {
            let mut got = None;
            link.datagram(self.ends.1, self.ends.0, |payload| {
                // SAFETY: `recv` writes into what it is given and says how much it wrote, which
                // is all of it that is read.
                let payload = unsafe { &mut *(payload as *mut [u8] as *mut [MaybeUninit<u8>]) };
                got = Some(self.host.recv(payload));
                got.as_ref()?.as_ref().ok().copied()
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
}

#[cfg(all(test, unix))]
mod tests {
    use std::io::Read as _;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, UdpSocket};
    use std::os::unix::net::UnixStream;

    use smoltcp::wire::{EthernetFrame, Ipv4Packet, Ipv6Packet, UdpPacket};

    use super::super::super::LAN;
    use super::*;

    /// What the guest sends goes out on the flow's host socket, and the reply reaches the guest
    /// from where it sent, its checksum whole, on v4 and v6.
    #[test]
    fn a_datagram_goes_out_and_its_reply_comes_back_from_where_it_was_sent() {
        let public_v6 = Ipv6Addr::new(0x2606, 0x4700, 0, 0, 0, 0, 0, 0x1111);
        let both: [Ends; 2] = [
            (
                (LAN.guest, 40000).into(),
                (Ipv4Addr::new(1, 1, 1, 1), 5000).into(),
            ),
            ((LAN.guest_v6, 40000).into(), (public_v6, 5000).into()),
        ];
        for ends in both {
            let host = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            let mut flow = UdpFlow::open(ends, host.local_addr().unwrap()).unwrap();
            flow.send(b"ping");
            let mut buf = [0; 16];
            let (n, from) = host.recv_from(&mut buf).unwrap();
            assert_eq!(&buf[..n], b"ping");
            host.send_to(b"pong", from).unwrap();

            let (ours, mut theirs) = UnixStream::pair().unwrap();
            let mut link = Link::new(ours.into()).unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            while !link.unsent() {
                assert!(Instant::now() < deadline, "no reply reached the link");
                flow.pump(&mut link, true);
            }
            link.flush().unwrap();
            drop(link);
            let mut got = Vec::new();
            theirs.read_to_end(&mut got).unwrap();

            let ip = EthernetFrame::new_checked(&got[4..]).unwrap().payload();
            let (src, dst, udp) = match ends.0 {
                SocketAddr::V4(_) => {
                    let ip = Ipv4Packet::new_checked(ip).unwrap();
                    (
                        IpAddr::from(ip.src_addr()),
                        IpAddr::from(ip.dst_addr()),
                        ip.payload(),
                    )
                }
                SocketAddr::V6(_) => {
                    let ip = Ipv6Packet::new_checked(ip).unwrap();
                    (
                        IpAddr::from(ip.src_addr()),
                        IpAddr::from(ip.dst_addr()),
                        ip.payload(),
                    )
                }
            };
            let udp = UdpPacket::new_checked(udp).unwrap();
            assert_eq!(SocketAddr::new(src, udp.src_port()), ends.1);
            assert_eq!(SocketAddr::new(dst, udp.dst_port()), ends.0);
            assert_eq!(udp.payload(), b"pong");
            assert!(udp.verify_checksum(&src.into(), &dst.into()));
        }
    }
}
