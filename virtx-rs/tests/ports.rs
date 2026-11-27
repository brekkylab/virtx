//! A sandbox's ports: reached on the loopback from `virtx-stdio`, and forwarded over HTTP from
//! `virtx-server-tcp`, each to what listens on it in the sandbox until the sandbox goes.

use std::{
    io::{BufRead, BufReader, Read, Write},
    net::TcpStream,
    sync::Mutex,
    thread,
    time::{Duration, Instant},
};

use virtx::{Error, PortForwarder, PortSide, PortSpec, SandboxCreateArgs, SandboxExecArgs};

mod common;

use common::ALPINE;

/// What `nc`, listening in the sandbox, says to a connection to `at`, once it listens.
fn said(at: impl std::net::ToSocketAddrs + Copy) -> Vec<u8> {
    loop {
        if let Ok(mut stream) = TcpStream::connect(at) {
            let mut said = Vec::new();
            let _ = stream.read_to_end(&mut said);
            if said.ends_with(b"hello\n") {
                return said;
            }
        }
        thread::sleep(Duration::from_millis(200));
    }
}

/// What the sandbox's server on 8000 answers `ping` sent to `at`, once it listens, for 30 seconds.
fn answer(at: impl std::net::ToSocketAddrs + Copy) -> String {
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut line = String::new();
    while Instant::now() < deadline {
        if let Ok(mut stream) = TcpStream::connect(at) {
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            line.clear();
            if stream.write_all(b"ping\n").is_ok() {
                let _ = BufReader::new(stream).read_line(&mut line);
            }
            if !line.is_empty() {
                return line;
            }
        }
        thread::sleep(Duration::from_millis(200));
    }
    panic!("nothing answered on the port in 30 seconds");
}

/// Serve on 8000 in a sandbox of `client`'s that opens it as `port` says, answering a line with
/// `hello` and the line, and answer the sandbox.
fn serving(client: &virtx::Client, port: PortSpec) -> virtx::Sandbox {
    common::alpine(client);
    let args = SandboxCreateArgs::new(ALPINE).port(port);
    let sandbox = client.create_sandbox(&args).wait().unwrap();
    let serve = SandboxExecArgs {
        timeout_ms: Some(30_000),
        ..SandboxExecArgs::new([
            "sh",
            "-c",
            "mkfifo /tmp/in; while true; do \
             cat /tmp/in | (read line; echo \"hello $line\") | nc -l -p 8000 > /tmp/in; done",
        ])
    };
    sandbox.exec(&serve).wait().unwrap();
    sandbox
}

#[test]
fn a_port_reaches_what_listens_on_it_until_the_sandbox_goes() {
    let client = common::client();
    // Saying what it is for.
    let port = PortSpec::new(8000).label("greeting");
    let sandbox = serving(&client, port);
    let opened = &sandbox.info().ports[0];
    assert_eq!(opened.label.as_deref(), Some("greeting"));
    let url = opened.url.as_deref().unwrap();
    let at = url.strip_prefix("http://").unwrap_or(url).to_owned();
    assert!(at.starts_with("127.0.0.1:"), "{at}");
    // Asked for again, it says the same: a loopback address only this machine reaches.
    let got = client.get_sandbox(sandbox.id()).wait().unwrap();
    assert_eq!(got.info().ports, std::slice::from_ref(opened));
    let listed = client.list_sandbox().wait().unwrap();
    let listed = listed.iter().find(|listed| listed.id() == sandbox.id());
    assert_eq!(listed.unwrap().info().ports, std::slice::from_ref(opened));

    assert_eq!(answer(at.as_str()), "hello ping\n");
    // Forwarded, it is reached at that address, or through one asked for in its place.
    let own = PortForwarder::open(&sandbox, 8000, PortSide::Sandbox, None);
    assert_eq!(own.wait().unwrap().local(), at);
    let asked = PortForwarder::open(&sandbox, 8000, PortSide::Sandbox, Some("127.0.0.1:0"));
    let asked = asked.wait().unwrap();
    assert_ne!(asked.local(), at);
    assert_eq!(answer(asked.local()), "hello ping\n");
    sandbox.remove().wait().unwrap();
    // Its listener goes with it, a moment after.
    let gone = until(|| TcpStream::connect(at.as_str()).is_err());
    assert!(gone, "its listener outlived it");
}

/// A large answer reaches a reader that stops reading for a while, without holding up another
/// port meanwhile; and the sandbox reaches its own published port, as any program in it would.
#[test]
fn a_port_carries_a_large_answer_to_a_reader_that_falls_behind() {
    let client = common::client();
    common::alpine(&client);
    let args = SandboxCreateArgs::new(ALPINE).port(8000).port(8001);
    let sandbox = client.create_sandbox(&args).wait().unwrap();
    // 8 MiB of one 16-byte line on 8001, and `hello` and the line sent on 8000.
    let serve = SandboxExecArgs {
        timeout_ms: Some(60_000),
        ..SandboxExecArgs::new([
            "sh",
            "-c",
            "while true; do nc -l -p 8001 -e sh -c 'yes 0123456789abcde | head -c 8388608'; \
             done & mkfifo /tmp/in; while true; do \
             cat /tmp/in | (read line; echo \"hello $line\") | nc -l -p 8000 > /tmp/in; done",
        ])
    };
    sandbox.exec(&serve).wait().unwrap();
    let at = |i: usize| {
        let url = sandbox.info().ports[i].url.clone().unwrap();
        url.strip_prefix("http://").unwrap_or(&url).to_owned()
    };

    // Its first bytes once it listens; a connection before then is closed with nothing said.
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut read = vec![0; 16];
    let mut stream = loop {
        assert!(Instant::now() < deadline, "nothing answered on 8001");
        if let Ok(mut stream) = TcpStream::connect(at(1).as_str())
            && stream.read_exact(&mut read).is_ok()
        {
            break stream;
        }
        thread::sleep(Duration::from_millis(200));
    };
    // Not read for a while, the rest held up behind it; the other port answers meanwhile.
    thread::sleep(Duration::from_secs(3));
    assert_eq!(answer(at(0).as_str()), "hello ping\n");
    stream.read_to_end(&mut read).unwrap();
    assert_eq!(read.len(), 8 << 20);
    assert!(read.chunks(16).all(|line| line == b"0123456789abcde\n"));

    let (lines, code) = common::run(
        &sandbox,
        &["sh", "-c", "echo ping | nc -w 5 127.0.0.1 8000"],
    );
    assert_eq!((lines, code), (vec!["hello ping".to_string()], 0));
    sandbox.remove().wait().unwrap();
}

/// A client that says it is done sending still hears the answer to what it sent.
#[test]
fn a_port_carries_a_half_close_through() {
    let client = common::client();
    common::alpine(&client);
    let args = SandboxCreateArgs::new(ALPINE).port(8002);
    let sandbox = client.create_sandbox(&args).wait().unwrap();
    // Counts what it is sent until the client half-closes, then says how much.
    let serve = SandboxExecArgs {
        timeout_ms: Some(60_000),
        ..SandboxExecArgs::new(["sh", "-c", "while true; do nc -l -p 8002 -e wc -c; done"])
    };
    sandbox.exec(&serve).wait().unwrap();
    let url = sandbox.info().ports[0].url.clone().unwrap();
    let at = url.strip_prefix("http://").unwrap_or(&url).to_owned();

    let deadline = Instant::now() + Duration::from_secs(30);
    let answered = loop {
        assert!(Instant::now() < deadline, "no answer after a half-close");
        if let Ok(mut stream) = TcpStream::connect(at.as_str()) {
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut answered = String::new();
            if stream.write_all(b"ping\n").is_ok()
                && stream.shutdown(std::net::Shutdown::Write).is_ok()
                && stream.read_to_string(&mut answered).is_ok()
                && !answered.is_empty()
            {
                break answered;
            }
        }
        thread::sleep(Duration::from_millis(200));
    };
    assert_eq!(answered.trim(), "5");
    sandbox.remove().wait().unwrap();
}

#[test]
fn a_port_over_http_is_forwarded_to_what_listens_on_it_until_the_sandbox_goes() {
    let client = common::tcp_client();
    let sandbox = serving(&client, PortSpec::new(8000).label("greeting"));
    assert_eq!(sandbox.info().ports[0].label.as_deref(), Some("greeting"));
    // A server over HTTP answers no address for it, made or asked for again, and the sandbox
    // found again forwards it as the one made does.
    assert_eq!(sandbox.info().ports[0].url, None);
    let got = client.get_sandbox(sandbox.id()).wait().unwrap();
    assert_eq!(got.info().ports, sandbox.info().ports);
    let forwarder = (PortForwarder::open(&got, 8000, PortSide::Sandbox, None).wait()).unwrap();
    assert_eq!(
        (forwarder.port(), forwarder.on()),
        (8000, PortSide::Sandbox)
    );
    let at: std::net::SocketAddr = forwarder.local().parse().unwrap();
    assert!(at.ip().is_loopback());

    assert_eq!(answer(at), "hello ping\n");
    sandbox.remove().wait().unwrap();
    // The forwarder stays, but what it reached is gone: each connection is closed at once.
    thread::sleep(Duration::from_millis(500));
    let mut read = Vec::new();
    let mut stream = TcpStream::connect(at).unwrap();
    assert_eq!(stream.read_to_end(&mut read).unwrap(), 0);
}

/// Whether `holds` does within a minute, looked at every 100 ms: a guard against a hang.
fn until(mut holds: impl FnMut() -> bool) -> bool {
    let began = Instant::now();
    while began.elapsed() < Duration::from_secs(60) {
        if holds() {
            return true;
        }
        thread::sleep(Duration::from_millis(100));
    }
    false
}

/// What the library warned of, as the `log` crate logs it under the target `virtx`.
static WARNED: Mutex<Vec<String>> = Mutex::new(Vec::new());

struct Warned;

impl log::Log for Warned {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.target() == "virtx" && metadata.level() == log::Level::Warn
    }

    fn log(&self, record: &log::Record) {
        if self.enabled(record.metadata()) {
            WARNED.lock().unwrap().push(record.args().to_string());
        }
    }

    fn flush(&self) {}
}

#[test]
fn a_forwarded_port_nothing_listens_on_says_so() {
    if log::set_logger(&Warned).is_ok() {
        log::set_max_level(log::LevelFilter::Warn);
    }
    let client = common::tcp_client();
    common::alpine(&client);
    let args = SandboxCreateArgs::new(ALPINE).port(7000);
    let sandbox = client.create_sandbox(&args).wait().unwrap();
    let forwarder = (PortForwarder::open(&sandbox, 7000, PortSide::Sandbox, None).wait()).unwrap();
    assert_eq!(forwarder.last_error(), None);

    let mut read = Vec::new();
    let mut stream = TcpStream::connect(forwarder.local()).unwrap();
    stream.read_to_end(&mut read).unwrap();
    assert!(read.is_empty(), "{read:?}");

    // Kept once the connection that failed has closed, and warned of.
    assert!(
        until(|| forwarder.last_error().is_some()),
        "no error was kept"
    );
    let Some(Error::Call(_, failed)) = forwarder.last_error() else {
        panic!("{:?}", forwarder.last_error());
    };
    let named = format!("port 7000, forwarded at {}: ", forwarder.local());
    assert!(failed.starts_with(&named), "{failed}");
    assert!(failed.contains("before the port sent a byte"), "{failed}");
    let warned = until(|| WARNED.lock().unwrap().contains(&failed));
    assert!(warned, "not warned of: {:?}", WARNED.lock().unwrap());

    // Once a connection reaches what listens there, the failure is no longer kept.
    let serve = SandboxExecArgs {
        timeout_ms: Some(30_000),
        ..SandboxExecArgs::new([
            "sh",
            "-c",
            "while true; do echo hello | nc -l -p 7000; done",
        ])
    };
    sandbox.exec(&serve).wait().unwrap();
    assert_eq!(said(forwarder.local()), b"hello\n");
    assert_eq!(forwarder.last_error(), None);
    sandbox.remove().wait().unwrap();
}
