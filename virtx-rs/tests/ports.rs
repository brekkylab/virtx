//! A sandbox's ports: reached on the loopback from `virtx-stdio`, and forwarded over HTTP from
//! `virtx-server-tcp`, each to what listens on it in the sandbox until the sandbox goes.

use std::{
    io::{BufRead, BufReader, Read, Write},
    net::TcpStream,
    thread,
    time::{Duration, Instant},
};

use virtx::{PortForwarder, SandboxCreateArgs, SandboxExecArgs};

mod common;

use common::ALPINE;

/// What the sandbox's server on 8000 answers `ping` sent to `at`, once it listens, for 30
/// seconds.
fn answer(at: impl std::net::ToSocketAddrs + Copy) -> String {
    let until = Instant::now() + Duration::from_secs(30);
    let mut line = String::new();
    while Instant::now() < until {
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

/// Serve on 8000 in a sandbox of `client`'s that opens it, answering a line with `hello` and
/// the line, and answer the sandbox.
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
    let sandbox = serving(&client);
    let url = &sandbox.info().ports[0].url;
    let at = url.strip_prefix("http://").unwrap_or(url).to_owned();
    assert!(at.starts_with("127.0.0.1:"), "{at}");
    // Answered only on its making: the URLs are as secret as what reaches them.
    let got = client.get_sandbox(sandbox.id()).wait().unwrap();
    assert!(got.info().ports.is_empty());

    assert_eq!(answer(at.as_str()), "hello ping\n");
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

    assert_eq!(answer(at), "hello ping\n");
    sandbox.remove().wait().unwrap();
    // The forwarder stays, but what it reached is gone: each connection is closed at once.
    thread::sleep(Duration::from_millis(500));
    let mut read = Vec::new();
    let mut stream = TcpStream::connect(at).unwrap();
    assert_eq!(stream.read_to_end(&mut read).unwrap(), 0);
}
