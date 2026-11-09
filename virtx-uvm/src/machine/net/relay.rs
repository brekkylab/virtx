//! A published port's connections, carried to wherever the guest listens on it.
//!
//! The listener a caller published is bound before the machine starts, at a port the caller
//! has already told somebody; the guest's own listener is bound by libkrun when a program in
//! the guest gets round to listening, at a port [`Policy`] chose. This is what joins the two:
//! a thread per published port, accepting, and a pair per connection, copying.
//!
//! A program that listened before its port was published listens in the guest alone: libkrun
//! was not asked to take that `listen`, and has nothing here to connect to. Its connections go
//! through the agent instead ([`Dial`]): asked over vsock to connect to the port on the
//! guest's own loopback, where that listener is. So a port is reached whenever its program
//! listened. One nothing listens on, either way, is closed -- what the guest would have
//! answered for a port nothing listens on.

use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, Ipv6Addr, Shutdown, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use super::policy::{Listeners, Policy};

/// How long the agent has to say whether it reached a port.
const DIALING: Duration = Duration::from_secs(5);

/// Where the agent is asked to connect to a port in the guest: the Unix socket libkrun listens
/// on and carries into the guest's vsock [`DIAL_PORT`](crate::machine::wire::DIAL_PORT). None
/// where there is none -- a host without Unix sockets in its standard library.
#[derive(Clone, Debug, Default)]
pub(crate) struct Dial(pub(crate) Option<PathBuf>);

/// Accept on `listener` for as long as the process lives, carrying each connection to the
/// guest's listener on `console`.
pub(super) fn spawn(
    console: u16,
    listener: TcpListener,
    policy: Arc<Policy>,
    dial: Dial,
) -> io::Result<()> {
    thread::Builder::new()
        .name(format!("relay-{console}"))
        .spawn(move || {
            for client in listener.incoming() {
                let Ok(client) = client else { continue };
                let listeners = policy.listener(console);
                let dial = dial.clone();
                let _ = thread::Builder::new()
                    .name(format!("relay-{console}-conn"))
                    .spawn(move || carry(client, console, listeners, &dial));
            }
        })?;
    Ok(())
}

/// The guest's end of a connection: its listener on this host's loopback, or the agent's
/// connection to the port in the guest.
enum Guest {
    Tcp(TcpStream),
    #[cfg(unix)]
    Agent(std::os::unix::net::UnixStream),
}

impl Guest {
    fn try_clone(&self) -> io::Result<Guest> {
        Ok(match self {
            Guest::Tcp(s) => Guest::Tcp(s.try_clone()?),
            #[cfg(unix)]
            Guest::Agent(s) => Guest::Agent(s.try_clone()?),
        })
    }

    fn shutdown_write(&self) {
        let _ = match self {
            Guest::Tcp(s) => s.shutdown(Shutdown::Write),
            #[cfg(unix)]
            Guest::Agent(s) => s.shutdown(Shutdown::Write),
        };
    }
}

impl Read for Guest {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Guest::Tcp(s) => s.read(buf),
            #[cfg(unix)]
            Guest::Agent(s) => s.read(buf),
        }
    }
}

impl Write for Guest {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Guest::Tcp(s) => s.write(buf),
            #[cfg(unix)]
            Guest::Agent(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Guest::Tcp(s) => s.flush(),
            #[cfg(unix)]
            Guest::Agent(s) => s.flush(),
        }
    }
}

/// The guest's listener on this host's loopback, if libkrun holds one for `console`: v4
/// first and v6 after, because the guest listened in whichever family its program chose, or in
/// both.
fn listened(listeners: Listeners) -> Option<Guest> {
    let v4 = listeners
        .v4
        .map(|port| TcpStream::connect((Ipv4Addr::LOCALHOST, port)));
    v4.filter(Result::is_ok)
        .or_else(|| {
            listeners
                .v6
                .map(|port| TcpStream::connect((Ipv6Addr::LOCALHOST, port)))
        })?
        .ok()
        .map(Guest::Tcp)
}

/// The port `console` in the guest, as the agent connects to it on the guest's loopback: for a
/// program that listened before the port was published.
#[cfg(unix)]
fn dialed(dial: &Dial, console: u16) -> Option<Guest> {
    use crate::machine::wire::DIALED;
    let mut agent = std::os::unix::net::UnixStream::connect(dial.0.as_ref()?).ok()?;
    agent.set_read_timeout(Some(DIALING)).ok()?;
    agent.write_all(&console.to_be_bytes()).ok()?;
    let mut answer = [0u8; 1];
    agent.read_exact(&mut answer).ok()?;
    agent.set_read_timeout(None).ok()?;
    (answer[0] == DIALED).then_some(Guest::Agent(agent))
}

#[cfg(not(unix))]
fn dialed(_: &Dial, _: u16) -> Option<Guest> {
    None
}

/// Join `client` to the guest's end of port `console`, in both directions, until either end is
/// done: its listener on this host's loopback, or the agent's connection to it in the guest.
///
/// The agent is asked only while the guest has not listened on the port since it was
/// published, which is when a listener in the guest alone is the one there is. Not once it
/// has: its `connect` to the port on the guest's loopback, finding nothing there, goes on to
/// this host, where the policy takes it to this relay again, which would ask the agent again,
/// for as long as anyone waits.
fn carry(client: TcpStream, console: u16, listeners: Option<Listeners>, dial: &Dial) {
    let guest = match listeners {
        Some(listeners) => listened(listeners),
        None => dialed(dial, console),
    };
    let Some(guest) = guest else {
        return;
    };
    let _ = client.set_nodelay(true);
    if let Guest::Tcp(s) = &guest {
        let _ = s.set_nodelay(true);
    }
    let (Ok(client_back), Ok(guest_back)) = (client.try_clone(), guest.try_clone()) else {
        return;
    };
    let back = thread::spawn(move || {
        let (mut from, mut to) = (guest_back, client_back);
        let _ = io::copy(&mut from, &mut to);
        let _ = to.shutdown(Shutdown::Write);
    });
    let (mut from, mut to) = (client, guest);
    let _ = io::copy(&mut from, &mut to);
    to.shutdown_write();
    let _ = back.join();
}
