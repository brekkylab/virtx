//! A sandbox's ports: reached on the loopback from `virtx-stdio`, and forwarded over HTTP from
//! `virtx-server-tcp`, each to what listens on it in the sandbox until the sandbox goes.

use std::{
    io::{BufRead, BufReader, Read, Write},
    net::TcpStream,
    sync::Mutex,
    thread,
    time::{Duration, Instant},
};

use virtx::{
    Error, InboundRule, PortForwarder, PortProtocol, PortSpec, SandboxCreateArgs, SandboxExecArgs,
    SandboxPort, Source,
};

mod common;

use common::ALPINE;

/// How long a test looks for what it waits on: a guard against a hang, not a measure of how
/// soon.
const HANG: Duration = Duration::from_secs(60);

/// Looked for until `holds` does, for as long as [`HANG`].
fn until(mut holds: impl FnMut() -> bool) -> bool {
    let began = Instant::now();
    while began.elapsed() < HANG {
        if holds() {
            return true;
        }
        thread::sleep(Duration::from_millis(100));
    }
    false
}

/// What `nc`, listening in the sandbox, says to a connection to `at`, once it listens.
fn said(at: impl std::net::ToSocketAddrs + Copy) -> Vec<u8> {
    let mut said = Vec::new();
    let heard = until(|| {
        said.clear();
        if let Ok(mut stream) = TcpStream::connect(at) {
            let _ = stream.read_to_end(&mut said);
        }
        said.ends_with(b"hello\n")
    });
    assert!(heard, "nothing listened: {said:?}");
    said
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
    // Over HTTP, which the loopback carries as it carries any TCP, and saying what it is for.
    let port = PortSpec::new(8000)
        .label("greeting")
        .protocol(PortProtocol::Http);
    let sandbox = serving(&client, port);
    let opened = &sandbox.info().ports[0];
    assert_eq!(
        (opened.label.as_deref(), opened.protocol, &opened.address),
        (Some("greeting"), PortProtocol::Http, &None)
    );
    let url = opened.url.as_deref().unwrap();
    let at = url.strip_prefix("http://").unwrap_or(url).to_owned();
    assert!(at.starts_with("127.0.0.1:"), "{at}");
    // Asked for again, it says what it is for, without the URL: that is as secret as what
    // reaches it, and answered only on its making.
    let asked = SandboxPort {
        url: None,
        ..opened.clone()
    };
    let got = client.get_sandbox(sandbox.id()).wait().unwrap();
    assert_eq!(got.info().ports, std::slice::from_ref(&asked));
    let listed = client.list_sandbox().wait().unwrap();
    let listed = listed.iter().find(|listed| listed.id() == sandbox.id());
    assert_eq!(listed.unwrap().info().ports, [asked]);

    assert_eq!(answer(at.as_str()), "hello ping\n");
    sandbox.remove().wait().unwrap();
    // Its listener goes with it.
    let gone = until(|| TcpStream::connect(at.as_str()).is_err());
    assert!(gone, "{at} still answers");
}

#[test]
fn a_port_over_http_is_forwarded_to_what_listens_on_it_until_the_sandbox_goes() {
    let client = common::tcp_client();
    let sandbox = serving(&client, PortSpec::new(8000).label("greeting"));
    assert_eq!(sandbox.info().ports[0].label.as_deref(), Some("greeting"));
    // Asked for again, without the URL, which carries the capability that reaches it.
    let got = client.get_sandbox(sandbox.id()).wait().unwrap();
    assert_eq!(
        got.info().ports,
        [SandboxPort {
            url: None,
            ..sandbox.info().ports[0].clone()
        }]
    );
    let url = sandbox.info().ports[0].url.as_deref().unwrap();
    let forwarder = PortForwarder::open(&client, url).unwrap();
    let at = forwarder.address();
    assert!(at.ip().is_loopback());

    assert_eq!(answer(at), "hello ping\n");
    sandbox.remove().wait().unwrap();
    // The forwarder stays, but what it reached is gone: each connection is closed at once.
    let closed = until(|| {
        let mut read = Vec::new();
        let mut stream = TcpStream::connect(at).unwrap();
        stream.read_to_end(&mut read).unwrap() == 0
    });
    assert!(closed, "{at} still reaches the sandbox");
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
    let args = SandboxCreateArgs {
        ports: vec![7000.into()],
        ..SandboxCreateArgs::new(ALPINE)
    };
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
    let Some(Error::Call(Source::Transport, failed)) = forwarder.last_error() else {
        panic!("{:?}", forwarder.last_error());
    };
    let named = format!("port 7000, forwarded at {}: ", forwarder.address());
    assert!(failed.starts_with(&named), "{failed}");
    assert!(failed.contains("before the port sent a byte"), "{failed}");
    assert!(!failed.contains(port.url.as_deref().unwrap()), "{failed}");
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

#[test]
fn a_port_says_what_it_is_for() {
    let client = common::client();
    common::alpine(&client);
    let mut args = SandboxCreateArgs::new(ALPINE);
    args.ports = vec![
        5900.into(),
        PortSpec {
            port: 5173,
            label: Some("dev server".into()),
            protocol: PortProtocol::Http,
            inbound: None,
        },
    ];
    let made = client.create_sandbox(&args).wait().unwrap();
    let described = |sandbox: &virtx::Sandbox| -> Vec<_> {
        sandbox
            .info()
            .ports
            .iter()
            .map(|p| (p.port, p.label.clone(), p.protocol))
            .collect()
    };
    let told = [
        (5900, None, PortProtocol::Tcp),
        (5173, Some("dev server".to_owned()), PortProtocol::Http),
    ];
    assert_eq!(described(&made), told);
    made.remove().wait().unwrap();
}

#[test]
fn inbound_rules_are_refused_by_a_server_that_keeps_none() {
    let client = common::client();
    common::alpine(&client);
    let mut ssh = PortSpec::new(22);
    ssh.inbound = Some(vec![InboundRule::new("10.0.0.0/8")]);
    let refused = client
        .create_sandbox(&SandboxCreateArgs::new(ALPINE).port(ssh))
        .wait()
        .unwrap_err();
    assert_eq!(
        (refused.status(), refused.code()),
        (Some(400), Some("invalid")),
        "{refused}"
    );
    assert!(
        refused.to_string().contains("not kept by this server"),
        "{refused}"
    );
}
