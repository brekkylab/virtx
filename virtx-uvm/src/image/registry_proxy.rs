//! A [`RegistryProxy`] a server runs for itself: an HTTP CONNECT proxy on this host's loopback
//! that lets a registry client through to the addresses the internet routes -- those a guest
//! with a network reaches -- and to the registries named to it.
//!
//! An image job runs in the server's own process, on the host, and a registry sends its client
//! on: to a token realm, down a redirect, to a layer's URL. So where a job may connect is
//! decided where each connection is made, not on the reference. A client asks for a host by
//! name; the name is resolved once, here, the connection is refused if any address it resolves
//! to is one the internet does not route, and it is made to an address that was checked.
//!
//! A request to be forwarded, which is how a client asks a proxy for an `http://` URL, is
//! refused: a tunnel is all that is carried.

use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::{JoinHandle, JoinSet};

use super::registry_client::{ProxyPass, ProxyRefusal, RegistryProxy};
use crate::machine::routed;

/// The most of a request's head that is read before the request is dropped.
const HEAD_LIMIT: usize = 16 << 10;

const ESTABLISHED: &[u8] = b"HTTP/1.1 200 Connection Established\r\n\r\n";
const FORBIDDEN: &[u8] = b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n";
const BAD_GATEWAY: &[u8] = b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n";

/// Why a host on a private address is refused.
const NOT_PUBLIC: &str = "it is not a public address";

/// Why a request to be forwarded is refused.
const NOT_A_TUNNEL: &str = "this server connects to registries over HTTPS alone";

/// The proxy the module tells of. Each client gets a listener of its own on the loopback, so
/// what is refused is told to the client that asked.
#[derive(Default)]
pub struct PublicRegistries {
    /// The registries let through wherever they are.
    private: Arc<[Named]>,
}

/// A registry as a reference names it: a host, at one port or at any.
#[derive(Debug, PartialEq, Eq)]
struct Named {
    /// An IPv6 address without its brackets.
    host: String,
    port: Option<u16>,
}

impl std::str::FromStr for Named {
    type Err = String;

    fn from_str(named: &str) -> Result<Named, String> {
        let refused = || format!("{named:?}: a registry is named `host` or `host:port`");
        let (host, port) = match named.rsplit_once(':') {
            // `[v6]:port` and `host:port`: any other colon is inside an IPv6 address.
            Some((host, port)) if host.ends_with(']') || !host.contains(':') => (host, Some(port)),
            _ => (named, None),
        };
        let host = unbracketed(host);
        let name = |host: &str| {
            !host.is_empty()
                && host
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_'))
        };
        if host.parse::<IpAddr>().is_err() && !name(host) {
            return Err(refused());
        }
        Ok(Named {
            host: host.to_owned(),
            port: match port {
                Some(port) => Some(port.parse().map_err(|_| refused())?),
                None => None,
            },
        })
    }
}

impl Named {
    /// Whether this is `host` at `port`: an address as itself however it is spelled, a name
    /// whatever its case.
    fn names(&self, host: &str, port: u16) -> bool {
        let host = match (self.host.parse::<IpAddr>(), host.parse::<IpAddr>()) {
            (Ok(named), Ok(asked)) => named == asked,
            _ => self.host.eq_ignore_ascii_case(host),
        };
        host && self.port.is_none_or(|named| named == port)
    }
}

/// `host` without the brackets an IPv6 address is written in beside a port.
fn unbracketed(host: &str) -> &str {
    host.strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host)
}

impl PublicRegistries {
    /// One that lets a client through to `private` wherever they are, as to every registry on
    /// a public address: each as a reference names its registry, `host` at any port or
    /// `host:port`. Refused, saying which, if one is neither.
    pub fn new<S: AsRef<str>>(private: impl IntoIterator<Item = S>) -> Result<Self, String> {
        let private = private
            .into_iter()
            .map(|named| named.as_ref().parse())
            .collect::<Result<_, _>>()?;
        Ok(PublicRegistries { private })
    }

    /// A listener on the loopback for one client's connections: its URL, and what accepts
    /// them and carries each for as long as it runs.
    fn listen(
        &self,
        refused: &Arc<Mutex<Option<ProxyRefusal>>>,
    ) -> io::Result<(String, JoinHandle<()>)> {
        let runtime = tokio::runtime::Handle::try_current().map_err(io::Error::other)?;
        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        listener.set_nonblocking(true)?;
        let url = format!("http://{}", listener.local_addr()?);
        let listener = TcpListener::from_std(listener)?;
        let (private, refused) = (Arc::clone(&self.private), Arc::clone(refused));
        let accepting = runtime.spawn(async move {
            let mut tunnels = JoinSet::new();
            loop {
                match listener.accept().await {
                    Ok((client, _)) => {
                        tunnels.spawn(tunnel(client, Arc::clone(&private), Arc::clone(&refused)));
                    }
                    // Out of descriptors, say: waited out, not spun on.
                    Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
                }
                while tunnels.try_join_next().is_some() {}
            }
        });
        Ok((url, accepting))
    }
}

impl RegistryProxy for PublicRegistries {
    fn pass(&self) -> Box<dyn ProxyPass> {
        let refused = Arc::new(Mutex::new(None));
        let (url, accepting) = match self.listen(&refused) {
            Ok((url, accepting)) => (url, Some(accepting)),
            Err(e) => {
                *refused.lock().unwrap() = Some(ProxyRefusal {
                    host: "a registry".into(),
                    why: format!("nothing listens for a connection to one: {e}"),
                });
                // Port 0 is no one's: a connection there fails where it starts.
                ("http://127.0.0.1:0".to_owned(), None)
            }
        };
        Box::new(Pass {
            url,
            refused,
            accepting,
        })
    }
}

struct Pass {
    url: String,
    refused: Arc<Mutex<Option<ProxyRefusal>>>,
    /// What accepts its connections, and with it every tunnel: none, where nothing listens.
    accepting: Option<JoinHandle<()>>,
}

impl ProxyPass for Pass {
    fn url(&self) -> &str {
        &self.url
    }

    fn refused(&self) -> Option<ProxyRefusal> {
        self.refused.lock().unwrap().take()
    }
}

impl Drop for Pass {
    fn drop(&mut self) {
        if let Some(accepting) = &self.accepting {
            accepting.abort();
        }
    }
}

/// Why a request got no connection.
enum Unreached {
    Refused(ProxyRefusal),
    /// It was let through, and there was no connecting: the request does not read, its name
    /// does not resolve, or its host does not answer.
    Failed,
}

/// One connection of a client's: its request answered, and the tunnel it asked for carried
/// until either end closes.
async fn tunnel(
    mut client: TcpStream,
    private: Arc<[Named]>,
    refused: Arc<Mutex<Option<ProxyRefusal>>>,
) {
    let Some((head, sent)) = head(&mut client).await else {
        return;
    };
    let answer = match reach(&head, &private).await {
        Ok(mut registry) => {
            let _ = async {
                client.write_all(ESTABLISHED).await?;
                registry.write_all(&sent).await?;
                tokio::io::copy_bidirectional(&mut client, &mut registry).await
            }
            .await;
            return;
        }
        // Before the client hears of it, so that it finds the refusal when it asks.
        Err(Unreached::Refused(refusal)) => {
            refused.lock().unwrap().get_or_insert(refusal);
            FORBIDDEN
        }
        Err(Unreached::Failed) => BAD_GATEWAY,
    };
    let _ = client.write_all(answer).await;
}

/// A request's head from `client`, and what it sent after: none, if there is no whole head
/// within [`HEAD_LIMIT`].
async fn head(client: &mut TcpStream) -> Option<(String, Vec<u8>)> {
    let mut read = Vec::new();
    let mut chunk = [0; 1024];
    loop {
        if let Some(end) = read.windows(4).position(|window| window == b"\r\n\r\n") {
            let sent = read.split_off(end + 4);
            return Some((String::from_utf8(read).ok()?, sent));
        }
        if read.len() >= HEAD_LIMIT {
            return None;
        }
        match client.read(&mut chunk).await {
            Ok(0) | Err(_) => return None,
            Ok(n) => read.extend_from_slice(&chunk[..n]),
        }
    }
}

/// The connection the request with `head` asks for, made: a `CONNECT` to a host that is one of
/// `private`, or whose every address the internet routes.
async fn reach(head: &str, private: &[Named]) -> Result<TcpStream, Unreached> {
    let mut line = head.lines().next().unwrap_or_default().split(' ');
    let (method, target) = (
        line.next().unwrap_or_default(),
        line.next().unwrap_or_default(),
    );
    if method != "CONNECT" {
        let host = target
            .strip_prefix("http://")
            .and_then(|rest| rest.split(['/', '?']).next())
            .unwrap_or(target);
        return Err(Unreached::Refused(ProxyRefusal {
            host: host.to_owned(),
            why: NOT_A_TUNNEL.to_owned(),
        }));
    }
    let (host, port) = target
        .rsplit_once(':')
        .and_then(|(host, port)| Some((unbracketed(host), port.parse::<u16>().ok()?)))
        .ok_or(Unreached::Failed)?;
    let addresses: Vec<SocketAddr> = tokio::net::lookup_host((host, port))
        .await
        .map_err(|_| Unreached::Failed)?
        .collect();
    if !private.iter().any(|named| named.names(host, port))
        && let Some(private) = addresses
            .iter()
            .find(|address| !routed(address.ip().to_canonical()))
    {
        let why = match host.parse::<IpAddr>() {
            Ok(_) => NOT_PUBLIC.to_owned(),
            Err(_) => format!("it resolves to {}, and {NOT_PUBLIC}", private.ip()),
        };
        return Err(Unreached::Refused(ProxyRefusal {
            host: target.to_owned(),
            why,
        }));
    }
    for address in addresses {
        if let Ok(registry) = TcpStream::connect(address).await {
            return Ok(registry);
        }
    }
    Err(Unreached::Failed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::registry_client::{Reference, RegistryClient};
    use crate::image::store::Store;

    /// A listener on the loopback, standing in for a registry there, that answers nothing.
    fn listener() -> (std::net::TcpListener, u16) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        (listener, port)
    }

    /// Whether `listener` has been connected to.
    fn connected(listener: &std::net::TcpListener) -> bool {
        match listener.accept() {
            Ok(_) => true,
            Err(e) => {
                assert_eq!(e.kind(), io::ErrorKind::WouldBlock);
                false
            }
        }
    }

    /// `request` sent through `pass`, as a client's connection: the status line answered, and
    /// the connection.
    async fn ask(pass: &dyn ProxyPass, request: &str) -> (String, TcpStream) {
        let at = pass.url().strip_prefix("http://").unwrap();
        let mut stream = TcpStream::connect(at).await.unwrap();
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut answer = Vec::new();
        while !answer.ends_with(b"\r\n\r\n") {
            answer.push(stream.read_u8().await.unwrap());
        }
        let answer = String::from_utf8(answer).unwrap();
        (answer.lines().next().unwrap().to_owned(), stream)
    }

    async fn connect(pass: &dyn ProxyPass, target: &str) -> (String, TcpStream) {
        let request = format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n");
        ask(pass, &request).await
    }

    #[test]
    fn a_registry_is_named_by_its_host_or_its_host_and_port() {
        let named = |host: &str, port| Named {
            host: host.to_owned(),
            port,
        };
        let read = |text: &str| text.parse::<Named>();
        assert_eq!(
            read("Registry.internal"),
            Ok(named("Registry.internal", None))
        );
        assert_eq!(
            read("registry.internal:5000"),
            Ok(named("registry.internal", Some(5000)))
        );
        assert_eq!(read("10.0.0.5:5000"), Ok(named("10.0.0.5", Some(5000))));
        assert_eq!(read("[fd00::1]:5000"), Ok(named("fd00::1", Some(5000))));
        assert_eq!(read("[fd00::1]"), Ok(named("fd00::1", None)));
        assert_eq!(read("fd00::1"), Ok(named("fd00::1", None)));
        for not in [
            "",
            "https://registry.internal",
            "registry.internal/team",
            "a:b",
            "a:70000",
            "a b",
        ] {
            assert_eq!(
                read(not),
                Err(format!(
                    "{not:?}: a registry is named `host` or `host:port`"
                ))
            );
        }

        assert!(named("Registry.internal", None).names("registry.INTERNAL", 443));
        assert!(named("registry.internal", Some(5000)).names("registry.internal", 5000));
        assert!(!named("registry.internal", Some(5000)).names("registry.internal", 443));
        assert!(!named("registry.internal", None).names("other.internal", 443));
        assert!(named("fd00::1", None).names("fd00:0:0:0:0:0:0:1", 443));
    }

    #[tokio::test]
    async fn what_a_guest_on_a_public_network_does_not_reach_is_refused() {
        let (registry, port) = listener();
        let proxy = PublicRegistries::default();
        let pass = proxy.pass();
        for host in [
            format!("127.0.0.1:{port}"),
            format!("0.0.0.0:{port}"),
            format!("[::1]:{port}"),
            format!("[::ffff:127.0.0.1]:{port}"),
            "10.0.0.1:443".to_owned(),
            "172.16.0.1:443".to_owned(),
            "192.168.0.1:443".to_owned(),
            "169.254.169.254:80".to_owned(),
            "100.64.0.1:443".to_owned(),
            "[fd00::1]:443".to_owned(),
            "[fe80::1]:443".to_owned(),
        ] {
            let (answer, _) = connect(&*pass, &host).await;
            assert_eq!(answer, "HTTP/1.1 403 Forbidden", "{host}");
            let why = NOT_PUBLIC.to_owned();
            assert_eq!(pass.refused(), Some(ProxyRefusal { host, why }));
        }

        // A name is refused for what it resolves to, whichever family comes first.
        let host = format!("localhost:{port}");
        let (answer, _) = connect(&*pass, &host).await;
        assert_eq!(answer, "HTTP/1.1 403 Forbidden");
        let refusal = pass.refused().unwrap();
        assert_eq!(refusal.host, host);
        assert!(
            ["127.0.0.1", "::1"]
                .iter()
                .any(|ip| refusal.why == format!("it resolves to {ip}, and {NOT_PUBLIC}")),
            "{}",
            refusal.why
        );

        assert!(!connected(&registry), "the registry was connected to");
        assert_eq!(pass.refused(), None);
    }

    #[tokio::test]
    async fn a_request_to_be_forwarded_is_refused() {
        let (registry, port) = listener();
        let proxy = PublicRegistries::new([format!("127.0.0.1:{port}")]).unwrap();
        let pass = proxy.pass();
        let host = format!("127.0.0.1:{port}");
        let request = format!("GET http://{host}/token?service=a HTTP/1.1\r\nHost: {host}\r\n\r\n");
        let (answer, _) = ask(&*pass, &request).await;
        assert_eq!(answer, "HTTP/1.1 403 Forbidden");
        let why = NOT_A_TUNNEL.to_owned();
        assert_eq!(pass.refused(), Some(ProxyRefusal { host, why }));
        assert!(!connected(&registry), "the registry was connected to");
    }

    #[tokio::test]
    async fn a_registry_named_is_reached_on_this_host_at_the_port_named() {
        let (registry, port) = listener();
        let (other, other_port) = listener();
        let proxy = PublicRegistries::new([format!("127.0.0.1:{port}")]).unwrap();
        let pass = proxy.pass();

        let (answer, mut client) = connect(&*pass, &format!("127.0.0.1:{port}")).await;
        assert_eq!(answer, "HTTP/1.1 200 Connection Established");
        let registry = TcpListener::from_std(registry).unwrap();
        let (mut carried, _) = registry.accept().await.unwrap();
        // Carried both ways.
        client.write_all(b"to the registry").await.unwrap();
        let mut read = [0; 15];
        carried.read_exact(&mut read).await.unwrap();
        assert_eq!(&read, b"to the registry");
        carried.write_all(b"and back").await.unwrap();
        let mut read = [0; 8];
        client.read_exact(&mut read).await.unwrap();
        assert_eq!(&read, b"and back");

        let (answer, _) = connect(&*pass, &format!("127.0.0.1:{other_port}")).await;
        assert_eq!(answer, "HTTP/1.1 403 Forbidden");
        assert!(!connected(&other), "another port was connected to");

        // Named without a port, it is reached at any.
        let proxy = PublicRegistries::new(["127.0.0.1"]).unwrap();
        let pass = proxy.pass();
        let (answer, _) = connect(&*pass, &format!("127.0.0.1:{other_port}")).await;
        assert_eq!(answer, "HTTP/1.1 200 Connection Established");
        assert_eq!(pass.refused(), None);
    }

    struct Scratch(Store);

    impl Scratch {
        fn new(name: &str) -> Scratch {
            let path = std::env::temp_dir().join(format!(
                "virtx-uvm-registry-proxy-{name}-{}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            Scratch(Store::new(path).unwrap())
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(self.0.root());
        }
    }

    /// `pull`, with a guard against a hang that is not a measure of anything.
    async fn pulled<T>(pull: impl Future<Output = T>) -> T {
        tokio::time::timeout(Duration::from_secs(60), pull)
            .await
            .expect("the pull went on")
    }

    #[tokio::test]
    async fn a_client_through_it_is_told_the_registry_on_the_loopback_is_not_reachable() {
        let scratch = Scratch::new("refused");
        let (listener, port) = listener();
        let registry = RegistryClient::new(scratch.0.clone(), None)
            .through(Some(&PublicRegistries::default()))
            .unwrap();
        let host = format!("127.0.0.1:{port}");
        let reference: Reference = format!("{host}/team/app:1").parse().unwrap();
        let e = pulled(registry.pull(&reference, |_| {})).await.unwrap_err();
        assert_eq!(
            e.to_string(),
            format!("{reference}: {host} is not reachable from this server: {NOT_PUBLIC}")
        );
        assert!(!connected(&listener), "the registry was connected to");
    }

    /// A proxy whose one pass was made already.
    struct Made(Mutex<Option<Box<dyn ProxyPass>>>);

    impl RegistryProxy for Made {
        fn pass(&self) -> Box<dyn ProxyPass> {
            self.0.lock().unwrap().take().unwrap()
        }
    }

    #[test]
    fn a_pass_made_where_nothing_can_listen_lets_nothing_through_and_says_so() {
        // Outside a runtime, where nothing accepts.
        let pass = PublicRegistries::default().pass();
        assert_eq!(pass.url(), "http://127.0.0.1:0");

        let scratch = Scratch::new("no-listener");
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let e = runtime.block_on(async {
            let registry = RegistryClient::new(scratch.0.clone(), None)
                .through(Some(&Made(Mutex::new(Some(pass)))))
                .unwrap();
            let reference: Reference = "registry.example/team/app:1".parse().unwrap();
            pulled(registry.pull(&reference, |_| {})).await.unwrap_err()
        });
        let e = e.to_string();
        let told = "registry.example/team/app:1: a registry is not reachable from this server: \
                    nothing listens for a connection to one: ";
        assert!(e.starts_with(told), "{e}");
    }

    /// Needs a network: `cargo test -p virtx-uvm -- --ignored`. Docker Hub's alpine from
    /// Google's mirror of it, which answers a request for a layer with a redirect.
    #[tokio::test]
    #[ignore]
    async fn a_public_image_is_pulled_through_it() {
        let scratch = Scratch::new("public");
        let registry = RegistryClient::new(scratch.0.clone(), None)
            .through(Some(&PublicRegistries::default()))
            .unwrap();
        let reference: Reference = "mirror.gcr.io/library/alpine:3.21".parse().unwrap();
        let (_, image) = registry.pull(&reference, |_| {}).await.unwrap();
        assert!(scratch.0.get_image(&image).unwrap().is_some());
    }
}
