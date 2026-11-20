//! A sandbox's ports: reached on the loopback from `virtx-stdio`, and forwarded over HTTP from
//! `virtx-server-tcp`, each to what listens on it in the sandbox until the sandbox goes.

use std::{io::Read, net::TcpStream, thread, time::Duration};

use virtx::{
    PortForwarder, PortProtocol, PortSpec, SandboxCreateArgs, SandboxExecArgs, SandboxPort,
};

mod common;

use common::ALPINE;

/// What `nc`, listening on 8000 in the sandbox, says to a connection to `at`, once it listens.
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

/// Serve `hello` on 8000 in a sandbox of `client`'s that opens it as `port` says, and answer
/// the sandbox.
fn serving(client: &virtx::Client, port: PortSpec) -> virtx::Sandbox {
    common::alpine(client);
    let args = SandboxCreateArgs::new(ALPINE).port(port);
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

    assert_eq!(said(at.as_str()), b"hello\n");
    sandbox.remove().wait().unwrap();
    // Its listener goes with it, a moment after.
    thread::sleep(Duration::from_millis(500));
    assert!(TcpStream::connect(at.as_str()).is_err());
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

    assert_eq!(said(at), b"hello\n");
    sandbox.remove().wait().unwrap();
    // The forwarder stays, but what it reached is gone: each connection is closed at once.
    thread::sleep(Duration::from_millis(500));
    let mut read = Vec::new();
    let mut stream = TcpStream::connect(at).unwrap();
    assert_eq!(stream.read_to_end(&mut read).unwrap(), 0);
}
