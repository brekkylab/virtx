//! A sandbox's ports: reached on the loopback from `virtx-stdio`, and forwarded over HTTP from
//! `virtx-server-tcp`, each to what listens on it in the sandbox until the sandbox goes.

use std::{
    io::Read,
    net::TcpStream,
    thread,
    time::{Duration, Instant},
};

use virtx::{
    InboundRule, PortForwarder, PortProtocol, PortSpec, SandboxCreateArgs, SandboxExecArgs,
    SandboxPortOpenArgs,
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

/// What `nc`, listening on 8000 in the sandbox, says to a connection to `at`, once it listens.
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

/// Serve `hello` on 8000 in a sandbox of `client`'s that opens it, and answer the sandbox.
fn serving(client: &virtx::Client) -> virtx::Sandbox {
    common::alpine(client);
    let args = SandboxCreateArgs {
        ports: vec![8000.into()],
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
    // Its listener goes with it.
    let gone = until(|| TcpStream::connect(at.as_str()).is_err());
    assert!(gone, "{at} still answers");
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
    let closed = until(|| {
        let mut read = Vec::new();
        let mut stream = TcpStream::connect(at).unwrap();
        stream.read_to_end(&mut read).unwrap() == 0
    });
    assert!(closed, "{at} still reaches the sandbox");
}

#[test]
fn a_port_says_what_it_is_for_and_is_opened_and_closed_as_the_sandbox_runs() {
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
    let described: Vec<_> = made
        .info()
        .ports
        .iter()
        .map(|p| (p.port, p.label.as_deref(), p.protocol))
        .collect();
    assert_eq!(
        described,
        [
            (5900, None, PortProtocol::Tcp),
            (5173, Some("dev server"), PortProtocol::Http)
        ]
    );
    let open = SandboxPortOpenArgs {
        label: Some("docs".into()),
        protocol: Some(PortProtocol::Http),
        ..Default::default()
    };
    let web = client
        .open_sandbox_port_with(made.id(), 8080, &open)
        .wait()
        .unwrap();
    assert_eq!(
        (web.port, web.label.as_deref(), web.protocol),
        (8080, Some("docs"), PortProtocol::Http)
    );
    let plain = made.open_port(9000).wait().unwrap();
    assert_eq!((plain.label, plain.protocol), (None, PortProtocol::Tcp));

    // Kept as those it was made with are, across a stop; closed, gone from them.
    made.stop().wait().unwrap();
    let started = made.start().wait().unwrap();
    let ports: Vec<_> = started.info().ports.iter().map(|p| p.port).collect();
    assert_eq!(ports, [5900, 5173, 8080, 9000]);
    made.close_port(9000).wait().unwrap();
    let started = client.start_sandbox(made.id()).wait().unwrap();
    let ports: Vec<_> = started.info().ports.iter().map(|p| p.port).collect();
    assert_eq!(ports, [5900, 5173, 8080]);
    let closed = client
        .close_sandbox_port(made.id(), 9000)
        .wait()
        .unwrap_err();
    assert_eq!(
        (closed.status(), closed.code()),
        (Some(404), Some("not_found")),
        "{closed}"
    );
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
    assert!(refused.to_string().contains("virtx cloud's"), "{refused}");
}
