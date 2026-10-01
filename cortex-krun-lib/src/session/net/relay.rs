//! A published port's connections, carried to wherever the guest listens on it.
//!
//! The listener a caller published is bound before the machine starts, at a port the caller
//! has already told somebody; the guest's own listener is bound by libkrun when a program in
//! the guest gets round to listening, at a port [`Policy`] chose. This is what joins the two:
//! a thread per published port, accepting, and a pair per connection, copying.
//!
//! A connection that arrives before anything in the guest is listening is closed, which is
//! what the guest would have answered for a port nothing listens on.

use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, Shutdown, TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;

use super::policy::{Listeners, Policy};

/// Accept on `listener` for as long as the process lives, carrying each connection to the
/// guest's listener on `console`.
pub(super) fn spawn(console: u16, listener: TcpListener, policy: Arc<Policy>) -> io::Result<()> {
    thread::Builder::new()
        .name(format!("relay-{console}"))
        .spawn(move || {
            for client in listener.incoming() {
                let Ok(client) = client else { continue };
                let Some(listeners) = policy.listener(console) else {
                    continue;
                };
                let _ = thread::Builder::new()
                    .name(format!("relay-{console}-conn"))
                    .spawn(move || carry(client, listeners));
            }
        })?;
    Ok(())
}

/// Join `client` to the guest's listener on this host's loopback, in both directions, until
/// either end is done.
///
/// v4 first and v6 after, because the guest listened in whichever family its program chose,
/// or in both.
fn carry(client: TcpStream, listeners: Listeners) {
    let v4 = listeners
        .v4
        .map(|port| TcpStream::connect((Ipv4Addr::LOCALHOST, port)));
    let Some(Ok(guest)) = v4.filter(Result::is_ok).or_else(|| {
        listeners
            .v6
            .map(|port| TcpStream::connect((Ipv6Addr::LOCALHOST, port)))
    }) else {
        return;
    };
    let _ = client.set_nodelay(true);
    let _ = guest.set_nodelay(true);
    let (Ok(client_back), Ok(guest_back)) = (client.try_clone(), guest.try_clone()) else {
        return;
    };
    let back = thread::spawn(move || copy(guest_back, client_back));
    copy(client, guest);
    let _ = back.join();
}

/// One direction, and the half-close that says it is over -- so the other direction keeps
/// going until its own end says the same.
fn copy(mut from: TcpStream, mut to: TcpStream) {
    let _ = io::copy(&mut from, &mut to);
    let _ = to.shutdown(Shutdown::Write);
}
