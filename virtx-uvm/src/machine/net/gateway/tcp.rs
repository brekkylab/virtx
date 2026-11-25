//! A TCP connection between the guest and this host: a smoltcp socket on the guest's side, a
//! host socket on the other, and the bytes moved between their buffers where they lie.
//!
//! The guest's half-close is passed on as a shutdown of the host socket's writing, and the
//! host's as a FIN to the guest, so each direction ends when its own sender says so. A failure
//! on the host's side resets the guest's.

use std::io::{self, Read as _, Write as _};
use std::net::{Shutdown, SocketAddr, SocketAddrV4};

use smoltcp::iface::{Context, SocketHandle, SocketSet};
use smoltcp::socket::tcp::{Socket as SmolSocket, SocketBuffer, State};
use smoltcp::wire::{IpAddress, IpListenEndpoint};
use socket2::{Domain, Protocol, Socket, Type};

use super::flow::Interest;

/// Each direction's buffer in smoltcp: room for a few of the link's segments in flight.
const BUFFER: usize = 256 << 10;

pub(super) struct TcpFlow {
    /// The guest's side, in smoltcp's sockets.
    handle: SocketHandle,
    host: Socket,
    /// The host socket's connect, not yet answered.
    connecting: bool,
    /// The host has written all it will, and the guest has been sent its FIN.
    host_done: bool,
    /// The guest has written all it will, and the host socket's writing is shut.
    guest_done: bool,
}

impl TcpFlow {
    /// A connection the guest is opening with a SYN to `to`: a socket listening there for
    /// smoltcp to take the SYN with, and a host socket connecting to `at`.
    pub(super) fn open(
        sockets: &mut SocketSet<'static>,
        to: SocketAddrV4,
        at: SocketAddr,
    ) -> io::Result<TcpFlow> {
        let host = Socket::new(Domain::for_address(at), Type::STREAM, Some(Protocol::TCP))?;
        host.set_nonblocking(true)?;
        host.set_tcp_nodelay(true)?;
        match host.connect(&at.into()) {
            Ok(()) => {}
            #[cfg(unix)]
            Err(e) if e.raw_os_error() == Some(libc::EINPROGRESS) => {}
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(e),
        }
        let mut socket = smol_socket();
        socket
            .listen(IpListenEndpoint {
                addr: Some((*to.ip()).into()),
                port: to.port(),
            })
            .map_err(io::Error::other)?;
        Ok(TcpFlow {
            handle: sockets.add(socket),
            host,
            connecting: true,
            host_done: false,
            guest_done: false,
        })
    }

    /// A connection a client of a published port opened, `host`: carried to `to` in the guest,
    /// from `from`.
    pub(super) fn accept(
        sockets: &mut SocketSet<'static>,
        cx: &mut Context,
        host: Socket,
        to: SocketAddrV4,
        from: SocketAddrV4,
    ) -> io::Result<TcpFlow> {
        host.set_nonblocking(true)?;
        host.set_tcp_nodelay(true)?;
        let mut socket = smol_socket();
        socket
            .connect(
                cx,
                (IpAddress::from(*to.ip()), to.port()),
                (IpAddress::from(*from.ip()), from.port()),
            )
            .map_err(io::Error::other)?;
        Ok(TcpFlow {
            handle: sockets.add(socket),
            host,
            connecting: false,
            host_done: false,
            guest_done: false,
        })
    }

    pub(super) fn host(&self) -> &Socket {
        &self.host
    }

    /// Move what each side has to the other.
    pub(super) fn pump(&mut self, sockets: &mut SocketSet) -> Interest {
        let socket = sockets.get_mut::<SmolSocket>(self.handle);
        if self.connecting {
            match self.host.take_error() {
                Ok(None) if self.host.peer_addr().is_ok() => self.connecting = false,
                Ok(None) => {
                    return Interest {
                        read: false,
                        write: true,
                    };
                }
                Ok(Some(_)) | Err(_) => return self.fail(socket),
            }
        }

        let mut write = false;
        while socket.can_recv() {
            match socket.recv(|buf| match (&self.host).write(buf) {
                Ok(n) => (n, Ok(())),
                Err(e) => (0, Err(e)),
            }) {
                Ok(Ok(())) => {}
                Ok(Err(e)) if e.kind() == io::ErrorKind::WouldBlock => {
                    write = true;
                    break;
                }
                Ok(Err(e)) if e.kind() == io::ErrorKind::Interrupted => {}
                _ => return self.fail(socket),
            }
        }
        if !self.guest_done
            && !socket.can_recv()
            && matches!(
                socket.state(),
                State::CloseWait | State::LastAck | State::Closing | State::TimeWait
            )
        {
            self.guest_done = true;
            let _ = self.host.shutdown(Shutdown::Write);
        }

        while !self.host_done && socket.can_send() {
            match socket.send(|buf| match (&self.host).read(buf) {
                Ok(n) => (n, Ok(n)),
                Err(e) => (0, Err(e)),
            }) {
                Ok(Ok(0)) => {
                    self.host_done = true;
                    socket.close();
                }
                Ok(Ok(_)) => {}
                Ok(Err(e)) if e.kind() == io::ErrorKind::WouldBlock => break,
                Ok(Err(e)) if e.kind() == io::ErrorKind::Interrupted => {}
                _ => return self.fail(socket),
            }
        }
        Interest {
            read: !self.host_done && socket.can_send(),
            write,
        }
    }

    /// Whether the connection is over on the guest's side, its last segment sent: closed and
    /// its RST out, waiting out its close, or still listening for a SYN it did not take.
    pub(super) fn over(&self, sockets: &SocketSet) -> bool {
        let socket = sockets.get::<SmolSocket>(self.handle);
        match socket.state() {
            State::Closed => socket.remote_endpoint().is_none(),
            State::TimeWait | State::Listen => true,
            _ => false,
        }
    }

    pub(super) fn close(self, sockets: &mut SocketSet) {
        sockets.remove(self.handle);
    }

    /// Reset the guest's side, whose RST goes out at the stack's next turn.
    fn fail(&mut self, socket: &mut SmolSocket) -> Interest {
        socket.abort();
        self.connecting = false;
        self.host_done = true;
        self.guest_done = true;
        Interest::default()
    }
}

fn smol_socket() -> SmolSocket<'static> {
    let mut socket = SmolSocket::new(
        SocketBuffer::new(vec![0; BUFFER]),
        SocketBuffer::new(vec![0; BUFFER]),
    );
    // What the host gives is sent as it comes, and acknowledged as it does: the link has no
    // cost per segment worth waiting to save.
    socket.set_nagle_enabled(false);
    socket.set_ack_delay(None);
    socket
}
