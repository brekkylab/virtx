//! A published port's connections, carried to the guest.
//!
//! The listener a caller published is bound before the machine starts, at a port the caller
//! has already told somebody; the guest is reached through a socket libkrun listens on, which
//! carries each connection to the guest port's vsock port, where the agent takes it to the
//! guest's loopback. This is what joins the two: a thread per published port, accepting, and a
//! pair per connection, copying.
//!
//! A connection to a port nothing in the guest listens on is closed, which is what the guest
//! would have answered.

use std::io;
use std::net::{Shutdown, TcpListener};
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;

/// Accept on `listener` for as long as the process lives, carrying each connection to guest
/// port `console` through `guest`, the socket libkrun listens on for it, in both directions
/// until either end is done.
pub(super) fn spawn(console: u16, listener: TcpListener, guest: PathBuf) -> io::Result<()> {
    let guest = Arc::new(guest);
    thread::Builder::new()
        .name(format!("relay-{console}"))
        .spawn(move || {
            for client in listener.incoming() {
                let Ok(client) = client else { continue };
                let guest = Arc::clone(&guest);
                let _ = thread::Builder::new()
                    .name(format!("relay-{console}-conn"))
                    .spawn(move || {
                        #[cfg(unix)]
                        let guest = std::os::unix::net::UnixStream::connect(&*guest);
                        #[cfg(windows)]
                        let guest = connect_unix(&guest);
                        let Ok(guest) = guest else { return };
                        let _ = client.set_nodelay(true);
                        let (Ok(client_back), Ok(guest_back)) =
                            (client.try_clone(), guest.try_clone())
                        else {
                            return;
                        };
                        // Each direction ends with the half-close that says so, and the other
                        // keeps going until its own end says the same.
                        let back = thread::spawn(move || {
                            let _ = io::copy(&mut &guest_back, &mut &client_back);
                            let _ = client_back.shutdown(Shutdown::Write);
                        });
                        let _ = io::copy(&mut &client, &mut &guest);
                        let _ = guest.shutdown(Shutdown::Write);
                        let _ = back.join();
                    });
            }
        })?;
    Ok(())
}

/// Windows has AF_UNIX stream sockets but std has no type for one, so the socket is held as a
/// `TcpStream`: every call a relay makes of it -- `recv`, `send`, `shutdown`, duplicating it --
/// is Winsock's for any stream socket.
#[cfg(windows)]
fn connect_unix(path: &std::path::Path) -> io::Result<std::net::TcpStream> {
    use std::os::windows::io::{FromRawSocket as _, RawSocket};
    use windows_sys::Win32::Networking::WinSock::{
        AF_UNIX, INVALID_SOCKET, SOCK_STREAM, SOCKADDR, SOCKADDR_UN, SOCKET_ERROR, WSAGetLastError,
        closesocket, connect, socket,
    };

    let bytes = path.to_str().ok_or(io::ErrorKind::InvalidInput)?.as_bytes();
    let mut addr: SOCKADDR_UN = unsafe { std::mem::zeroed() };
    // The last byte stays the terminating NUL.
    if bytes.len() >= addr.sun_path.len() {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    addr.sun_family = AF_UNIX;
    for (slot, &byte) in addr.sun_path.iter_mut().zip(bytes) {
        *slot = byte as _;
    }
    // SAFETY: plain values; Winsock is started, by the listener this relays from.
    let socket = unsafe { socket(AF_UNIX.into(), SOCK_STREAM, 0) };
    if socket == INVALID_SOCKET {
        return Err(io::Error::from_raw_os_error(unsafe { WSAGetLastError() }));
    }
    let len = std::mem::size_of::<SOCKADDR_UN>() as i32;
    // SAFETY: a socket just made, and an address of the length given.
    if unsafe { connect(socket, &addr as *const _ as *const SOCKADDR, len) } == SOCKET_ERROR {
        let e = io::Error::from_raw_os_error(unsafe { WSAGetLastError() });
        // SAFETY: the socket just made, owned by nothing else.
        unsafe { closesocket(socket) };
        return Err(e);
    }
    // SAFETY: a connected socket, owned by nothing else, which the stream now closes.
    Ok(unsafe { std::net::TcpStream::from_raw_socket(socket as RawSocket) })
}
