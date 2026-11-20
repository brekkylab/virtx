//! A sandbox's ports: reached on the loopback from `virtx-stdio`, and forwarded over HTTP from
//! `virtx-server-tcp`, each to what listens on it in the sandbox until the sandbox goes.

use std::{
    io::Read,
    net::TcpStream,
    sync::Mutex,
    thread,
    time::{Duration, Instant},
};

use virtx::{Error, PortForwarder, SandboxCreateArgs, SandboxExecArgs};

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

/// Serve `hello` on 8000 in a sandbox of `client`'s that opens it, and answer the sandbox.
fn serving(client: &virtx::Client) -> virtx::Sandbox {
    common::alpine(client);
    let args = SandboxCreateArgs {
        ports: vec![8000],
        ..SandboxCreateArgs::new(ALPINE)
    };
    let sandbox = client.create_sandbox(&args).wait().unwrap();
    let serve = SandboxExecArgs {
        timeout_ms: Some(30_000),
        ..SandboxExecArgs::new([
            "sh",
            "-c",
            "while true; do echo hello | nc -l -p 8000; done",
        ])
    };
    sandbox.exec(&serve).wait().unwrap();
    sandbox
}

#[test]
fn a_port_reaches_what_listens_on_it_until_the_sandbox_goes() {
    let client = common::client();
    let sandbox = serving(&client);
    let url = &sandbox.info().ports[0].url;
    let at = url.strip_prefix("http://").unwrap_or(url).to_owned();
    assert!(at.starts_with("127.0.0.1:"), "{at}");
    // Answered only on its making: the URLs are as secret as what reaches them.
    let got = client.get_sandbox(sandbox.id()).wait().unwrap();
    assert!(got.info().ports.is_empty());

    assert_eq!(said(at.as_str()), b"hello\n");
    sandbox.remove().wait().unwrap();
    // Its listener goes with it, a moment after.
    thread::sleep(Duration::from_millis(500));
    assert!(TcpStream::connect(at.as_str()).is_err());
}

#[test]
fn a_port_over_http_is_forwarded_to_what_listens_on_it_until_the_sandbox_goes() {
    let client = common::tcp_client();
    let sandbox = serving(&client);
    let forwarder = PortForwarder::open(&client, &sandbox.info().ports[0].url).unwrap();
    let at = forwarder.address();
    assert!(at.ip().is_loopback());

    assert_eq!(said(at), b"hello\n");
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
    let port = &sandbox.info().ports[0];
    let forwarder = PortForwarder::open(&client, port).unwrap();
    assert_eq!(forwarder.last_error(), None);

    let mut read = Vec::new();
    let mut stream = TcpStream::connect(forwarder.address()).unwrap();
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
    let named = format!("port 7000, forwarded at {}: ", forwarder.address());
    assert!(failed.starts_with(&named), "{failed}");
    assert!(failed.contains("before the port sent a byte"), "{failed}");
    assert!(!failed.contains(&port.url), "{failed}");
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
    assert_eq!(said(forwarder.address()), b"hello\n");
    assert_eq!(forwarder.last_error(), None);
    sandbox.remove().wait().unwrap();
}
