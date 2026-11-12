//! The proxy a [`RegistryClient`](super::registry_client::RegistryClient) held to public
//! registries connects through: an HTTP CONNECT proxy on the loopback, in this process. It
//! resolves each host it is asked for itself and connects only to an address of it that
//! [`is_public`], the very address it checked, so a name that resolves elsewhere a second time
//! gains nothing. Each hop of a pull or a push -- the registry, its token realm, a redirect, a
//! layer's URL -- is a CONNECT of its own, so each is checked. A request for a plain `http://`
//! URL, which comes as the request itself rather than a CONNECT, is refused outright.
//!
//! Each client passes a token of its own as the proxy's credentials: a connection without one
//! is refused, and what the proxy refused a client is kept for that client's error.

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};

/// Whether `ip` is an address on the internet: not unspecified, the host's own, a private
/// network's, a link's (the cloud's metadata address, 169.254.169.254, among them), carrier-grade
/// NAT's, documentation's, a multicast group, broadcast or otherwise reserved -- nor any of these
/// in a form IPv6 carries an IPv4 address in.
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_public_v4(ip),
        IpAddr::V6(ip) => is_public_v6(ip),
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    let reserved = a == 0 // this network, 0.0.0.0 among it
        || a == 10 // private
        || (a == 100 && (64..128).contains(&b)) // carrier-grade NAT
        || a == 127 // loopback
        || (a == 169 && b == 254) // link-local
        || (a == 172 && (16..32).contains(&b)) // private
        || (a == 192 && b == 0 && (c == 0 || c == 2)) // protocol assignments, documentation
        || (a == 192 && b == 168) // private
        || (a == 198 && (b == 18 || b == 19)) // benchmarking
        || (a == 198 && b == 51 && c == 100) // documentation
        || (a == 203 && b == 0 && c == 113) // documentation
        || a >= 224; // multicast, reserved, broadcast
    !reserved
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_public_v4(v4);
    }
    let s = ip.segments();
    let v4 = |high: u16, low: u16| Ipv4Addr::from((u32::from(high) << 16) | u32::from(low));
    // NAT64's well-known prefix and 6to4 reach the IPv4 address they carry.
    if s[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
        return is_public_v4(v4(s[6], s[7]));
    }
    if s[0] == 0x2002 {
        return is_public_v4(v4(s[1], s[2]));
    }
    // Global unicast, 2000::/3, and of it neither documentation's nor Teredo's, which hides the
    // IPv4 address it reaches. Everything else -- unspecified, loopback, IPv4-compatible,
    // unique-local, link-local, multicast -- is outside it.
    (s[0] & 0xe000) == 0x2000 && !(s[0] == 0x2001 && (s[1] == 0 || s[1] == 0xdb8))
}

/// What the proxy refused a client: the host and port it asked for, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Refusal {
    pub host: String,
    pub why: String,
}

/// A proxy serving on the loopback, on threads of its own, for as long as the process runs.
pub(crate) struct Proxy {
    addr: SocketAddr,
    /// Which addresses it connects to: [`is_public`], but in a test.
    allowed: fn(IpAddr) -> bool,
    /// Each client's `Proxy-Authorization`, and the first thing it was refused.
    clients: Mutex<HashMap<String, Option<Refusal>>>,
}

/// The longest request head read: a CONNECT's is a line and a few headers.
const HEAD_LIMIT: usize = 16 << 10;

impl Proxy {
    /// The process's proxy, started on first use: [`is_public`]'s addresses only.
    pub(crate) fn shared() -> io::Result<Arc<Proxy>> {
        static SHARED: Mutex<Option<Arc<Proxy>>> = Mutex::new(None);
        let mut shared = SHARED.lock().unwrap();
        if let Some(proxy) = &*shared {
            return Ok(Arc::clone(proxy));
        }
        let proxy = Proxy::start(is_public)?;
        *shared = Some(Arc::clone(&proxy));
        Ok(proxy)
    }

    /// A proxy on a port of its own that connects to the addresses `allowed` lets through.
    pub(crate) fn start(allowed: fn(IpAddr) -> bool) -> io::Result<Arc<Proxy>> {
        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        listener.set_nonblocking(true)?;
        let proxy = Arc::new(Proxy {
            addr: listener.local_addr()?,
            allowed,
            clients: Mutex::default(),
        });
        // A runtime of its own, so that it outlives whichever runtime first asked for it.
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("virtx-registry-proxy")
            .enable_all()
            .build()?;
        let serving = Arc::clone(&proxy);
        std::thread::Builder::new()
            .name("virtx-registry-proxy".into())
            .spawn(move || {
                runtime.block_on(async move {
                    let Ok(listener) = TcpListener::from_std(listener) else {
                        return;
                    };
                    loop {
                        let Ok((stream, _)) = listener.accept().await else {
                            // Out of descriptors, say: a moment for some to close.
                            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                            continue;
                        };
                        let proxy = Arc::clone(&serving);
                        tokio::spawn(async move { proxy.serve(stream).await });
                    }
                })
            })?;
        Ok(proxy)
    }

    /// A client's pass through this proxy, until it drops.
    pub(crate) fn pass(self: &Arc<Proxy>) -> Pass {
        let token = uuid::Uuid::new_v4().simple().to_string();
        let encoded = base64::engine::general_purpose::STANDARD.encode(format!("virtx:{token}"));
        let header = format!("Basic {encoded}");
        self.clients.lock().unwrap().insert(header.clone(), None);
        Pass {
            proxy: Arc::clone(self),
            url: format!("http://virtx:{token}@{}", self.addr),
            header,
        }
    }

    /// The first thing the client with `header` was refused, if it was not yet.
    fn refuse(&self, header: &str, host: &str, why: String) {
        if let Some(refused @ None) = self.clients.lock().unwrap().get_mut(header) {
            *refused = Some(Refusal {
                host: host.to_owned(),
                why,
            });
        }
    }

    async fn serve(&self, mut client: TcpStream) -> io::Result<()> {
        let mut head = Vec::new();
        let end = loop {
            if let Some(end) = head.windows(4).position(|w| w == b"\r\n\r\n") {
                break end + 4;
            }
            if head.len() > HEAD_LIMIT {
                return answer(&mut client, "431 Request Header Fields Too Large").await;
            }
            let mut chunk = [0; 4096];
            match client.read(&mut chunk).await? {
                0 => return Ok(()),
                n => head.extend_from_slice(&chunk[..n]),
            }
        };
        let early = head.split_off(end);
        let head = String::from_utf8_lossy(&head);
        let mut lines = head.split("\r\n");
        let mut request = lines.next().unwrap_or_default().split(' ');
        let (method, target) = (
            request.next().unwrap_or_default(),
            request.next().unwrap_or_default(),
        );
        let header = lines
            .filter_map(|line| line.split_once(':'))
            .find(|(name, _)| name.trim().eq_ignore_ascii_case("proxy-authorization"))
            .map(|(_, value)| value.trim().to_owned())
            .filter(|header| self.clients.lock().unwrap().contains_key(header));
        let Some(header) = header else {
            return answer(&mut client, "407 Proxy Authentication Required").await;
        };

        if !method.eq_ignore_ascii_case("CONNECT") {
            let host = target
                .split_once("://")
                .map_or(target, |(_, rest)| rest)
                .split('/')
                .next()
                .unwrap_or_default();
            self.refuse(
                &header,
                host,
                format!("plain http:// ({target}) is not allowed"),
            );
            return answer(&mut client, "403 Forbidden").await;
        }
        let Ok(resolved) = tokio::net::lookup_host(target).await else {
            return answer(&mut client, "502 Bad Gateway").await;
        };
        let (allowed, refused): (Vec<SocketAddr>, Vec<SocketAddr>) =
            resolved.partition(|addr| (self.allowed)(addr.ip()));
        if allowed.is_empty() {
            if let Some(addr) = refused.first() {
                let why = format!("it is at {}, which is not a public address", addr.ip());
                self.refuse(&header, target, why);
                return answer(&mut client, "403 Forbidden").await;
            }
            return answer(&mut client, "502 Bad Gateway").await;
        }
        let mut upstream = None;
        for addr in allowed {
            if let Ok(stream) = TcpStream::connect(addr).await {
                upstream = Some(stream);
                break;
            }
        }
        let Some(mut upstream) = upstream else {
            return answer(&mut client, "502 Bad Gateway").await;
        };
        client
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await?;
        upstream.write_all(&early).await?;
        tokio::io::copy_bidirectional(&mut client, &mut upstream).await?;
        Ok(())
    }
}

/// `status`, with nothing after, and the connection closed.
async fn answer(client: &mut TcpStream, status: &str) -> io::Result<()> {
    let response = format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    client.write_all(response.as_bytes()).await?;
    client.shutdown().await
}

/// A client's way through a [`Proxy`]: the URL to name it by, with the client's token as its
/// credentials, and what the proxy refused it.
pub(crate) struct Pass {
    proxy: Arc<Proxy>,
    url: String,
    /// The `Proxy-Authorization` the URL's credentials are sent as.
    header: String,
}

impl Pass {
    pub(crate) fn url(&self) -> &str {
        &self.url
    }

    /// The first thing the proxy refused this client since this was last asked, if anything.
    pub(crate) fn refused(&self) -> Option<Refusal> {
        let mut clients = self.proxy.clients.lock().unwrap();
        clients.get_mut(&self.header).and_then(Option::take)
    }
}

impl Drop for Pass {
    fn drop(&mut self) {
        self.proxy.clients.lock().unwrap().remove(&self.header);
    }
}

impl std::fmt::Debug for Pass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Pass")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn public(ip: &str) -> bool {
        is_public(ip.parse().unwrap())
    }

    #[test]
    fn only_addresses_on_the_internet_are_public() {
        for refused in [
            // IPv4: unspecified and this network, loopback, private, carrier-grade NAT,
            // link-local and the metadata address, documentation, benchmarking, multicast,
            // reserved and broadcast.
            "0.0.0.0",
            "0.1.2.3",
            "127.0.0.1",
            "127.255.255.254",
            "10.0.0.1",
            "10.255.255.255",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.0.1",
            "192.168.255.255",
            "100.64.0.1",
            "100.127.255.255",
            "169.254.0.1",
            "169.254.169.254",
            "169.254.169.253",
            "192.0.0.1",
            "192.0.2.1",
            "198.18.0.1",
            "198.19.255.255",
            "198.51.100.1",
            "203.0.113.1",
            "224.0.0.1",
            "239.255.255.250",
            "240.0.0.1",
            "255.255.255.255",
            // IPv6: unspecified, loopback, IPv4-compatible, unique-local (fd00:ec2::254 is the
            // metadata address's), link-local, multicast, documentation, Teredo.
            "::",
            "::1",
            "::7f00:1",
            "fc00::1",
            "fd00:ec2::254",
            "fe80::1",
            "febf::1",
            "ff02::1",
            "2001:db8::1",
            "2001:0:4136:e378:8000:63bf:3fff:fdd2",
            // IPv4-mapped, NAT64 and 6to4 forms of refused IPv4 addresses.
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "::ffff:169.254.169.254",
            "::ffff:100.64.0.1",
            "::ffff:0.0.0.0",
            "::ffff:255.255.255.255",
            "64:ff9b::a9fe:a9fe",
            "64:ff9b::7f00:1",
            "2002:7f00:1::1",
            "2002:a9fe:a9fe::1",
        ] {
            assert!(!public(refused), "{refused} passed");
        }
        for passed in [
            "8.8.8.8",
            "1.1.1.1",
            "142.250.196.110",
            "100.63.255.255",
            "100.128.0.0",
            "172.15.255.255",
            "172.32.0.0",
            "169.253.255.255",
            "192.169.0.1",
            "223.255.255.254",
            "2001:4860:4860::8888",
            "2600:1f18::1",
            "::ffff:8.8.8.8",
            "64:ff9b::808:808",
            "2002:808:808::1",
        ] {
            assert!(public(passed), "{passed} refused");
        }
    }

    /// `request` sent to `proxy`, and the status line it answered with.
    async fn ask(proxy: &Proxy, request: &str) -> (TcpStream, String) {
        let mut stream = TcpStream::connect(proxy.addr).await.unwrap();
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut answered = Vec::new();
        while !answered.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            assert_eq!(stream.read(&mut byte).await.unwrap(), 1, "{answered:?}");
            answered.push(byte[0]);
        }
        let status = String::from_utf8(answered).unwrap();
        (stream, status.lines().next().unwrap().to_owned())
    }

    fn connect(target: &str, pass: &Pass) -> String {
        format!(
            "CONNECT {target} HTTP/1.1\r\nHost: {target}\r\nProxy-Authorization: {}\r\n\r\n",
            pass.header
        )
    }

    #[tokio::test]
    async fn a_connect_to_a_host_not_on_a_public_address_is_refused_and_never_made() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let proxy = Proxy::start(is_public).unwrap();
        let pass = proxy.pass();

        let (_, status) = ask(&proxy, &connect(&format!("127.0.0.1:{port}"), &pass)).await;
        assert_eq!(status, "HTTP/1.1 403 Forbidden");
        assert_eq!(
            pass.refused(),
            Some(Refusal {
                host: format!("127.0.0.1:{port}"),
                why: "it is at 127.0.0.1, which is not a public address".into(),
            })
        );
        assert_eq!(pass.refused(), None);
        for target in [
            format!("localhost:{port}"),
            format!("[::ffff:127.0.0.1]:{port}"),
            "169.254.169.254:80".to_owned(),
        ] {
            let (_, status) = ask(&proxy, &connect(&target, &pass)).await;
            assert_eq!(status, "HTTP/1.1 403 Forbidden", "{target}");
            assert_eq!(pass.refused().map(|r| r.host), Some(target));
        }
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            io::ErrorKind::WouldBlock,
            "the proxy connected"
        );

        // Plain HTTP is refused whatever it names.
        let (_, status) = ask(
            &proxy,
            &format!(
                "GET http://example.com/v2/ HTTP/1.1\r\nHost: example.com\r\nProxy-Authorization: {}\r\n\r\n",
                pass.header
            ),
        )
        .await;
        assert_eq!(status, "HTTP/1.1 403 Forbidden");
        assert_eq!(
            pass.refused().map(|r| r.host),
            Some("example.com".to_owned())
        );

        // Without a client's pass, or with one dropped, nothing at all.
        let (_, status) = ask(&proxy, "CONNECT example.com:443 HTTP/1.1\r\n\r\n").await;
        assert_eq!(status, "HTTP/1.1 407 Proxy Authentication Required");
        let request = connect("example.com:443", &pass);
        drop(pass);
        let (_, status) = ask(&proxy, &request).await;
        assert_eq!(status, "HTTP/1.1 407 Proxy Authentication Required");
    }

    #[tokio::test]
    async fn a_connect_to_an_allowed_address_is_made_and_carries_both_ways() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target = listener.local_addr().unwrap().to_string();
        // The loopback let through, as a public address would be.
        let proxy = Proxy::start(|_| true).unwrap();
        let pass = proxy.pass();

        let (mut stream, status) = ask(&proxy, &connect(&target, &pass)).await;
        assert_eq!(status, "HTTP/1.1 200 Connection Established");
        let (mut upstream, _) = listener.accept().await.unwrap();
        stream.write_all(b"ping").await.unwrap();
        let mut read = [0; 4];
        upstream.read_exact(&mut read).await.unwrap();
        assert_eq!(&read, b"ping");
        upstream.write_all(b"pong").await.unwrap();
        stream.read_exact(&mut read).await.unwrap();
        assert_eq!(&read, b"pong");
        assert_eq!(pass.refused(), None);
    }
}
