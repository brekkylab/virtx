//! A sandbox's client ports, on this host: the relay its machines' connections to them reach,
//! and the takers that carry each on to a client.
//!
//! ```text
//!   machine ──greeting──► relay ──► the port's waiting connections ──► ClientPortTaker::next
//!           ◄──verdict───────────────────────────────────────────────── ClientConnection::accept
//! ```
//!
//! The relay listens on this host's loopback for as long as the sandbox is, across its
//! machines, so each is told one address. A connection opens with the relay's greeting -- its
//! key, which only the sandbox's machines are handed, and the port -- and waits for a taker of
//! its port. It is answered once a taker has reached what the port is on its client's machine
//! and [accepts](ClientConnection::accept) it, and the process's SYN in the guest with it. One
//! no taker accepts within [`HOLD`] is refused then: dropped, which the machine reads as a
//! refusal. A port has no taker between one a client lets go of and the next it takes, so a
//! connection waits as well while the port has had one within [`HOLD`]; to a port no one has
//! taken for that long, it is refused at once.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;
use tokio::task::AbortHandle;

use super::Error;
use crate::machine::{CLIENT_ACCEPTED, CLIENT_GREETING, ClientRelay};

/// How long a connection to a taken port waits for a taker to accept it.
pub const HOLD: Duration = Duration::from_secs(10);

/// How long a connection to the relay has to say its greeting.
const GREETING_TIMEOUT: Duration = Duration::from_secs(5);

/// How many connections to a port wait at once, past which one is refused.
const WAITING: usize = 64;

/// A sandbox's client ports: the relay, and each port's takers and waiting connections.
pub(crate) struct ClientPorts {
    relay: ClientRelay,
    shared: Arc<Shared>,
    accepting: AbortHandle,
}

struct Shared {
    ports: HashMap<u16, Port>,
    closed: AtomicBool,
    next: AtomicU64,
}

#[derive(Default)]
struct Port {
    /// How many takers it has now.
    takers: AtomicUsize,
    /// When a taker of it last let it go.
    let_go: Mutex<Option<Instant>>,
    /// Its connections not yet taken, oldest first, each by its number.
    waiting: Mutex<VecDeque<(u64, TcpStream)>>,
    /// Told of each connection that comes to wait, and of the sandbox's closing.
    arrived: Notify,
}

impl ClientPorts {
    /// A relay for `ports`, listening on this host's loopback from now on, on the runtime this
    /// is called in.
    pub(crate) fn open(ports: &[u16]) -> Result<ClientPorts, Error> {
        let failed = |source| Error::Io {
            context: "opening the client ports' relay".into(),
            source,
        };
        let runtime =
            tokio::runtime::Handle::try_current().map_err(|e| failed(io::Error::other(e)))?;
        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).map_err(failed)?;
        listener.set_nonblocking(true).map_err(failed)?;
        let relay = ClientRelay {
            address: listener.local_addr().map_err(failed)?,
            ports: ports.to_vec(),
            key: format!(
                "{}{}",
                uuid::Uuid::new_v4().simple(),
                uuid::Uuid::new_v4().simple()
            ),
        };
        let shared = Arc::new(Shared {
            ports: ports.iter().map(|&port| (port, Port::default())).collect(),
            closed: AtomicBool::new(false),
            next: AtomicU64::new(0),
        });
        let (key, accepted) = (relay.key.clone(), Arc::clone(&shared));
        let accepting = runtime.spawn(async move {
            let Ok(listener) = TcpListener::from_std(listener) else {
                return;
            };
            loop {
                let Ok((connection, _)) = listener.accept().await else {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                };
                tokio::spawn(wait(connection, key.clone(), Arc::clone(&accepted)));
            }
        });
        Ok(ClientPorts {
            relay,
            shared,
            accepting: accepting.abort_handle(),
        })
    }

    /// What its machines are told of it.
    pub(crate) fn relay(&self) -> &ClientRelay {
        &self.relay
    }

    /// A taker of `port`, until it drops, beside any others: refused
    /// [`NotFound`](Error::NotFound) for a port it was not made with.
    pub(crate) fn take(&self, port: u16) -> Result<ClientPortTaker, Error> {
        let Some(taken) = self.shared.ports.get(&port) else {
            return Err(Error::NotFound(format!(
                "this sandbox was made with no port {port} on local"
            )));
        };
        taken.takers.fetch_add(1, Ordering::Relaxed);
        Ok(ClientPortTaker {
            port,
            shared: Arc::clone(&self.shared),
        })
    }

    /// Stop the relay, refuse every connection waiting, and end every taker's waiting.
    pub(crate) fn close(&self) {
        self.accepting.abort();
        self.shared.closed.store(true, Ordering::SeqCst);
        for port in self.shared.ports.values() {
            port.waiting.lock().unwrap().clear();
            port.arrived.notify_waiters();
        }
    }
}

impl Drop for ClientPorts {
    fn drop(&mut self) {
        self.close();
    }
}

/// Read `connection`'s greeting, and have it wait for a taker of the port it names: dropped,
/// and so refused, for a greeting without `key`, a port no one has taken within [`HOLD`], or
/// one with as many waiting as it holds; and once it has waited [`HOLD`] untaken.
async fn wait(mut connection: TcpStream, key: String, shared: Arc<Shared>) {
    let mut greeting = [0u8; CLIENT_GREETING];
    let said = connection.read_exact(&mut greeting);
    if !matches!(
        tokio::time::timeout(GREETING_TIMEOUT, said).await,
        Ok(Ok(_))
    ) {
        return;
    }
    let (said_key, port) = greeting.split_at(key.len());
    if !same(said_key, key.as_bytes()) {
        return;
    }
    let port = u16::from_be_bytes([port[0], port[1]]);
    let Some(taken) = shared.ports.get(&port) else {
        return;
    };
    let number = shared.next.fetch_add(1, Ordering::Relaxed);
    {
        let mut waiting = taken.waiting.lock().unwrap();
        let untaken = taken.takers.load(Ordering::Relaxed) == 0
            && taken
                .let_go
                .lock()
                .unwrap()
                .is_none_or(|let_go| let_go.elapsed() >= HOLD);
        if untaken || waiting.len() >= WAITING {
            return;
        }
        waiting.push_back((number, connection));
    }
    taken.arrived.notify_one();
    tokio::time::sleep(HOLD).await;
    let mut waiting = taken.waiting.lock().unwrap();
    waiting.retain(|(waited, _)| *waited != number);
}

/// A taker of a client port: the connections made to it, one by one. Dropped, it takes no more;
/// a connection that comes while the port has no taker waits for the next as one waiting does,
/// and once the port has had none for [`HOLD`], one is refused at once.
pub struct ClientPortTaker {
    port: u16,
    shared: Arc<Shared>,
}

impl ClientPortTaker {
    /// The port it takes.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The next connection to the port no other taker has, as it comes; `None` once the
    /// sandbox is closed.
    pub async fn next(&self) -> Option<ClientConnection> {
        let port = &self.shared.ports[&self.port];
        loop {
            let arrived = port.arrived.notified();
            if self.shared.closed.load(Ordering::SeqCst) {
                return None;
            }
            if let Some((_, connection)) = port.waiting.lock().unwrap().pop_front() {
                return Some(ClientConnection(connection));
            }
            arrived.await;
        }
    }
}

impl Drop for ClientPortTaker {
    fn drop(&mut self) {
        let port = &self.shared.ports[&self.port];
        *port.let_go.lock().unwrap() = Some(Instant::now());
        port.takers.fetch_sub(1, Ordering::Relaxed);
        // One it was told of and did not take is another's.
        port.arrived.notify_one();
    }
}

/// A connection a process in the sandbox made to a client port, not yet answered. Dropped, it
/// is refused, as one to a port nothing listens on is.
pub struct ClientConnection(TcpStream);

impl ClientConnection {
    /// Answer it, the process's `connect` succeeding, and carry it on: its bytes are the
    /// process's from here.
    pub async fn accept(mut self) -> io::Result<TcpStream> {
        self.0.write_all(&[CLIENT_ACCEPTED]).await?;
        let _ = self.0.set_nodelay(true);
        Ok(self.0)
    }
}

/// Whether `a` and `b` are the same, in time that does not say where they differ.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0, |differ, (a, b)| differ | (a ^ b)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A connection to the relay as a machine's is: its greeting said.
    async fn connect(relay: &ClientRelay, port: u16) -> TcpStream {
        let mut connection = TcpStream::connect(relay.address).await.unwrap();
        connection.write_all(&relay.greeting(port)).await.unwrap();
        connection
    }

    /// What a connection is answered: its verdict, or `None` for the end.
    async fn verdict(connection: &mut TcpStream) -> Option<u8> {
        let mut verdict = [0u8];
        match connection.read(&mut verdict).await.unwrap() {
            0 => None,
            _ => Some(verdict[0]),
        }
    }

    #[tokio::test]
    async fn a_connection_to_a_taken_port_is_carried_once_its_taker_accepts_it() {
        let ports = ClientPorts::open(&[9222]).unwrap();
        let taker = ports.take(9222).unwrap();
        let mut machine = connect(ports.relay(), 9222).await;
        let mut carried = taker.next().await.unwrap().accept().await.unwrap();
        assert_eq!(verdict(&mut machine).await, Some(CLIENT_ACCEPTED));
        machine.write_all(b"hi").await.unwrap();
        let mut said = [0u8; 2];
        carried.read_exact(&mut said).await.unwrap();
        assert_eq!(&said, b"hi");
    }

    #[tokio::test]
    async fn a_connection_not_accepted_or_no_ones_is_refused() {
        let ports = ClientPorts::open(&[9222]).unwrap();
        // No one takes it.
        let mut machine = connect(ports.relay(), 9222).await;
        assert_eq!(verdict(&mut machine).await, None);

        let taker = ports.take(9222).unwrap();
        let mut machine = connect(ports.relay(), 9222).await;
        drop(taker.next().await.unwrap());
        assert_eq!(verdict(&mut machine).await, None);
    }

    /// As a client over HTTP takes a port: one taker for each connection, let go of as the
    /// connection is said, and another taken after.
    #[tokio::test]
    async fn connections_made_at_once_wait_for_the_next_taker() {
        let ports = ClientPorts::open(&[9222]).unwrap();
        // Between one taker and the next.
        drop(ports.take(9222).unwrap());
        let mut machines = Vec::new();
        for _ in 0..5 {
            machines.push(connect(ports.relay(), 9222).await);
        }
        for _ in 0..5 {
            let taker = ports.take(9222).unwrap();
            let connection = taker.next().await.unwrap();
            drop(taker);
            tokio::time::sleep(Duration::from_millis(50)).await;
            connection.accept().await.unwrap();
        }
        for mut machine in machines {
            assert_eq!(verdict(&mut machine).await, Some(CLIENT_ACCEPTED));
        }
    }

    #[tokio::test]
    async fn each_connection_goes_to_one_taker_of_many() {
        let ports = ClientPorts::open(&[9222]).unwrap();
        let (first, second) = (ports.take(9222).unwrap(), ports.take(9222).unwrap());
        let _one = connect(ports.relay(), 9222).await;
        let _two = connect(ports.relay(), 9222).await;
        let (one, two) = tokio::join!(first.next(), second.next());
        assert!(one.is_some() && two.is_some());
    }

    #[tokio::test]
    async fn a_connection_without_the_key_reaches_no_taker() {
        let ports = ClientPorts::open(&[9222]).unwrap();
        let _taker = ports.take(9222).unwrap();
        let other = ClientRelay {
            key: "x".repeat(64),
            ..ports.relay().clone()
        };
        let mut stranger = connect(&other, 9222).await;
        assert_eq!(verdict(&mut stranger).await, None);
    }

    #[tokio::test]
    async fn a_port_it_was_not_made_with_is_not_taken() {
        let ports = ClientPorts::open(&[9222]).unwrap();
        assert!(matches!(ports.take(5037), Err(Error::NotFound(_))));
    }

    #[tokio::test]
    async fn closing_ends_every_takers_waiting() {
        let ports = ClientPorts::open(&[9222]).unwrap();
        let taker = ports.take(9222).unwrap();
        let waiting = taker.next();
        ports.close();
        assert!(waiting.await.is_none());
    }
}
