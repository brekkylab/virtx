//! Where a sandbox's ports are reached, across the machines that come and go under it.
//!
//! A machine publishes each of its [`ports`](super::Machine::ports) on this host's loopback,
//! at an address of its own choosing, for as long as it runs ([`Report::Published`]). A
//! sandbox boots a machine for each command, so what stays is not that address but the port:
//! [`Ports::connect`] reaches whichever machine publishes it now.
//!
//! [`Report::Published`]: super::Report::Published

use std::collections::BTreeMap;
use std::io;
use std::net::SocketAddr;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::net::TcpStream;

/// The addresses the running machines publish their ports at.
#[derive(Debug, Default)]
pub struct Ports {
    /// Each running machine's, by the number its publication was given: oldest first.
    running: Mutex<BTreeMap<u64, Vec<(u16, SocketAddr)>>>,
    next: AtomicU64,
}

/// A machine's addresses, published until this drops: when the machine has ended.
#[must_use = "dropped, the addresses are withdrawn at once"]
pub struct Publication<'a> {
    ports: &'a Ports,
    number: u64,
}

impl Ports {
    /// Publish `addresses`, as a machine's [`Report::Published`](super::Report::Published)
    /// says them, until the publication drops.
    pub fn publish(&self, addresses: Vec<(u16, SocketAddr)>) -> Publication<'_> {
        let number = self.next.fetch_add(1, Ordering::Relaxed);
        self.running.lock().unwrap().insert(number, addresses);
        Publication {
            ports: self,
            number,
        }
    }

    /// A connection to `port` in the machine that published it last. Refused when no
    /// machine runs, as a port nothing listens on is.
    pub async fn connect(&self, port: u16) -> io::Result<TcpStream> {
        let address = self
            .running
            .lock()
            .unwrap()
            .values()
            .rev()
            .find_map(|addresses| {
                addresses
                    .iter()
                    .find(|(published, _)| *published == port)
                    .map(|(_, address)| *address)
            })
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::ConnectionRefused,
                    format!("no machine runs to reach port {port} in"),
                )
            })?;
        TcpStream::connect(address).await
    }
}

impl Drop for Publication<'_> {
    fn drop(&mut self) {
        self.ports.running.lock().unwrap().remove(&self.number);
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::TcpListener;

    use super::*;

    #[tokio::test]
    async fn a_port_reaches_the_machine_that_published_it_last_while_it_runs() {
        let ports = Ports::default();
        assert_eq!(
            ports.connect(3000).await.unwrap_err().kind(),
            io::ErrorKind::ConnectionRefused
        );

        let (first, second) = (
            TcpListener::bind("127.0.0.1:0").await.unwrap(),
            TcpListener::bind("127.0.0.1:0").await.unwrap(),
        );
        let older = ports.publish(vec![(3000, first.local_addr().unwrap())]);
        let newer = ports.publish(vec![(3000, second.local_addr().unwrap())]);
        let reached = |listener: &TcpListener| listener.local_addr().unwrap().port();

        let mut stream = ports.connect(3000).await.unwrap();
        let (mut accepted, _) = second.accept().await.unwrap();
        stream.write_all(b"hi").await.unwrap();
        let mut said = [0u8; 2];
        accepted.read_exact(&mut said).await.unwrap();
        assert_eq!(&said, b"hi");
        assert_eq!(stream.peer_addr().unwrap().port(), reached(&second));

        drop(newer);
        let stream = ports.connect(3000).await.unwrap();
        assert_eq!(stream.peer_addr().unwrap().port(), reached(&first));
        assert!(ports.connect(5900).await.is_err());

        drop(older);
        assert!(ports.connect(3000).await.is_err());
    }
}
