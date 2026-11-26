//! The stack's end of the guest's virtio-net device: a stream socket libkrun's `unixstream`
//! backend writes each frame the guest sends into, and reads each frame for the guest from --
//! its length as four big-endian bytes, then the Ethernet frame.
//!
//! Read in as much as the socket has, many frames at once, and each handed to the stack where
//! it lies; written out as the stack makes them in place, a turn's frames in one write.

use std::io::{self, Read as _, Write as _};
use std::net::{IpAddr, SocketAddr};

use smoltcp::phy::{self, Checksum, ChecksumCapabilities, DeviceCapabilities, Medium};
use smoltcp::time::Instant;
use smoltcp::wire::{
    EthernetAddress, EthernetFrame, EthernetProtocol, EthernetRepr, IpProtocol, Ipv4Packet,
    Ipv4Repr, Ipv6Packet, Ipv6Repr, UdpPacket,
};
use socket2::Socket;

use super::LAN;

/// The length before each frame.
const HEADER: usize = 4;

/// The longest frame either way: the MTU and the Ethernet header.
pub(super) const MAX_FRAME: usize = LAN.mtu as usize + 14;

/// How much is read at once, and the most of the device's frames held unread.
const RX_CAPACITY: usize = 1 << 20;

/// How much may wait for the device to take before the stack makes no more.
const TX_BACKLOG: usize = 4 << 20;

/// The end libkrun's device is given: one of a socket pair, or on Windows, where libkrun
/// connects once the guest's driver is up, the path it connects to.
pub(super) enum DeviceEnd {
    #[cfg(unix)]
    Fd(std::os::fd::OwnedFd),
    #[cfg(windows)]
    Path(String),
}

/// The stack's end, until the device has connected to it.
pub(super) struct Pending {
    #[cfg(unix)]
    socket: Socket,
    #[cfg(windows)]
    listener: Socket,
    #[cfg(windows)]
    path: std::path::PathBuf,
}

/// Where the machine's process `pid` listens for its device on Windows: one link a process, at
/// a path its host finds again to remove once the process has ended.
#[cfg(windows)]
pub(crate) fn path(pid: u32) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("virtx-net-{pid}.sock"))
}

/// The two ends of a new link: on Windows, its device to connect at `path`.
pub(super) fn open(#[cfg(windows)] path: std::path::PathBuf) -> io::Result<(DeviceEnd, Pending)> {
    #[cfg(unix)]
    {
        let (ours, theirs) = std::os::unix::net::UnixStream::pair()?;
        Ok((
            DeviceEnd::Fd(theirs.into()),
            Pending {
                socket: ours.into(),
            },
        ))
    }
    #[cfg(windows)]
    {
        let named = path
            .to_str()
            .ok_or(io::ErrorKind::InvalidFilename)?
            .to_owned();
        let _ = std::fs::remove_file(&path);
        let listener = Socket::new(socket2::Domain::UNIX, socket2::Type::STREAM, None)?;
        listener.bind(&socket2::SockAddr::unix(&path)?)?;
        listener.listen(1)?;
        Ok((DeviceEnd::Path(named), Pending { listener, path }))
    }
}

impl Pending {
    /// The link, once the device is at its end of it: at once on unix, and on Windows once
    /// libkrun has connected, after which the path goes.
    pub(super) fn connect(self) -> io::Result<Link> {
        #[cfg(unix)]
        let socket = self.socket;
        #[cfg(windows)]
        let (socket, _) = self.listener.accept()?;
        #[cfg(windows)]
        let _ = std::fs::remove_file(&self.path);
        Link::new(socket)
    }
}

pub(super) struct Link {
    socket: Socket,
    rx: Box<[u8]>,
    rx_start: usize,
    rx_end: usize,
    tx: Vec<u8>,
    tx_sent: usize,
}

/// The device's side: the socket, read and written.
impl Link {
    pub(super) fn new(socket: Socket) -> io::Result<Link> {
        socket.set_nonblocking(true)?;
        Ok(Link {
            socket,
            rx: vec![0; RX_CAPACITY].into_boxed_slice(),
            rx_start: 0,
            rx_end: 0,
            tx: Vec::new(),
            tx_sent: 0,
        })
    }

    /// What the stack waits on for the device.
    pub(super) fn socket(&self) -> &Socket {
        &self.socket
    }

    /// Read what the device has written. `Ok(false)` once it has closed its end.
    pub(super) fn fill(&mut self) -> io::Result<bool> {
        self.rx.copy_within(self.rx_start..self.rx_end, 0);
        self.rx_end -= self.rx_start;
        self.rx_start = 0;
        while self.rx_end < self.rx.len() {
            match (&self.socket).read(&mut self.rx[self.rx_end..]) {
                Ok(0) => return Ok(false),
                Ok(n) => self.rx_end += n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(true),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        match self.next() {
            Some(_) => Ok(true),
            None => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "a frame longer than the link reads",
            )),
        }
    }

    /// Write what the device will take of what waits for it.
    pub(super) fn flush(&mut self) -> io::Result<()> {
        while self.tx_sent < self.tx.len() {
            match (&self.socket).write(&self.tx[self.tx_sent..]) {
                Ok(n) => self.tx_sent += n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        self.tx.drain(..self.tx_sent);
        self.tx_sent = 0;
        Ok(())
    }

    /// Whether anything waits for the device to take it.
    pub(super) fn unsent(&self) -> bool {
        self.tx_sent < self.tx.len()
    }
}

/// The stack's side: frames from the guest, taken one at a time, and frames for it, made in
/// place. smoltcp takes and makes them through [`phy::Device`] below.
impl Link {
    /// The next frame from the guest, left for [`skip`](Self::skip) or smoltcp to take.
    pub(super) fn peek(&self) -> Option<&[u8]> {
        self.next().map(|(at, len)| &self.rx[at..at + len])
    }

    pub(super) fn skip(&mut self) {
        if let Some((at, len)) = self.next() {
            self.rx_start = at + len;
        }
    }

    /// Make a frame for the guest of at most `max` bytes in place: `make` says how long it is,
    /// or `None` for no frame after all.
    pub(super) fn send(&mut self, max: usize, make: impl FnOnce(&mut [u8]) -> Option<usize>) {
        let at = self.tx.len();
        self.tx.resize(at + HEADER + max, 0);
        match make(&mut self.tx[at + HEADER..]) {
            Some(len) => {
                self.tx[at..at + HEADER].copy_from_slice(&(len as u32).to_be_bytes());
                self.tx.truncate(at + HEADER + len);
            }
            None => self.tx.truncate(at),
        }
    }

    /// Make a datagram for the guest from `from` to `to`, in place: `payload` writes what it
    /// carries where it lies and says how long it is, or `None` for no datagram.
    pub(super) fn datagram(
        &mut self,
        from: SocketAddr,
        to: SocketAddr,
        payload: impl FnOnce(&mut [u8]) -> Option<usize>,
    ) {
        self.send(MAX_FRAME, |frame| {
            let at = EthernetFrame::<&[u8]>::header_len();
            let ip = if to.is_ipv4() { 20 } else { 40 };
            let len = payload(&mut frame[at + ip + 8..])?;
            let (ethertype, at) = match (from.ip(), to.ip()) {
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
                _ => unreachable!("a datagram's ends are of one family"),
            };
            EthernetRepr {
                src_addr: EthernetAddress(LAN.gateway_mac),
                dst_addr: EthernetAddress(LAN.guest_mac),
                ethertype,
            }
            .emit(&mut EthernetFrame::new_unchecked(&mut frame[..]));
            let mut packet = UdpPacket::new_unchecked(&mut frame[at..at + 8 + len]);
            packet.set_src_port(from.port());
            packet.set_dst_port(to.port());
            packet.set_len((8 + len) as u16);
            packet.fill_checksum(&from.ip().into(), &to.ip().into());
            Some(at + 8 + len)
        });
    }

    /// Whether as much waits for the device as the stack may leave there, so it makes no more.
    pub(super) fn full(&self) -> bool {
        self.tx.len() - self.tx_sent >= TX_BACKLOG
    }

    /// Where the next whole frame read in lies, and how long it is.
    fn next(&self) -> Option<(usize, usize)> {
        let have = &self.rx[self.rx_start..self.rx_end];
        let len = u32::from_be_bytes(have.get(..HEADER)?.try_into().unwrap()) as usize;
        (have.len() >= HEADER + len).then_some((self.rx_start + HEADER, len))
    }
}

impl phy::Device for Link {
    type RxToken<'a> = Rx<'a>;
    type TxToken<'a> = Tx<'a>;

    fn receive(&mut self, _: Instant) -> Option<(Rx<'_>, Tx<'_>)> {
        let (at, len) = self.next()?;
        self.rx_start = at + len;
        Some((Rx(&self.rx[at..at + len]), Tx(&mut self.tx)))
    }

    fn transmit(&mut self, _: Instant) -> Option<Tx<'_>> {
        (!self.full()).then_some(Tx(&mut self.tx))
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ethernet;
        caps.max_transmission_unit = MAX_FRAME;
        // Not checked on the way in: the link is memory, and a guest offloading its checksums
        // sends them unfinished.
        let mut checksum = ChecksumCapabilities::default();
        checksum.ipv4 = Checksum::Tx;
        checksum.tcp = Checksum::Tx;
        checksum.udp = Checksum::Tx;
        checksum.icmpv4 = Checksum::Tx;
        caps.checksum = checksum;
        caps
    }
}

pub(super) struct Rx<'a>(&'a [u8]);

impl phy::RxToken for Rx<'_> {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(self.0)
    }
}

pub(super) struct Tx<'a>(&'a mut Vec<u8>);

impl phy::TxToken for Tx<'_> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let at = self.0.len();
        self.0.extend_from_slice(&(len as u32).to_be_bytes());
        self.0.resize(at + HEADER + len, 0);
        f(&mut self.0[at + HEADER..])
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::net::UnixStream;

    use super::*;

    /// Frames written in pieces, across reads, come out whole and in order; the device's end
    /// closing is told once what it wrote is read.
    #[test]
    fn frames_come_out_whole_however_they_arrive() {
        let (ours, mut theirs) = UnixStream::pair().unwrap();
        let mut link = Link::new(ours.into()).unwrap();
        let framed: Vec<u8> = [&b"one"[..], b"", b"three"]
            .iter()
            .flat_map(|frame| [&(frame.len() as u32).to_be_bytes()[..], frame].concat())
            .collect();

        theirs.write_all(&framed[..5]).unwrap();
        assert!(link.fill().unwrap());
        assert_eq!(link.peek(), None);
        theirs.write_all(&framed[5..]).unwrap();
        assert!(link.fill().unwrap());
        for frame in [&b"one"[..], b"", b"three"] {
            assert_eq!(link.peek(), Some(frame));
            link.skip();
        }
        assert_eq!(link.peek(), None);

        drop(theirs);
        assert!(!link.fill().unwrap());
    }

    /// What the stack makes reaches the device framed, a frame made `None` not at all.
    #[test]
    fn frames_go_out_framed() {
        let (ours, mut theirs) = UnixStream::pair().unwrap();
        let mut link = Link::new(ours.into()).unwrap();
        link.send(16, |buf| {
            buf[..2].copy_from_slice(b"hi");
            Some(2)
        });
        link.send(16, |_| None);
        assert!(link.unsent());
        link.flush().unwrap();
        assert!(!link.unsent());
        drop(link);
        let mut got = Vec::new();
        theirs.read_to_end(&mut got).unwrap();
        assert_eq!(got, [0, 0, 0, 2, b'h', b'i']);
    }
}
