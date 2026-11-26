//! A sandbox's client ports: a process in it reaching a listener of this machine's at
//! `10.0.2.2` while this client takes the port -- through `virtx-stdio`, on this machine, which
//! reaches it itself, and over HTTP from `virtx-server-tcp`.

use std::{
    io::{BufRead, BufReader, Write},
    net::TcpListener,
    thread,
    time::{Duration, Instant},
};

use virtx::{ClientPortForwarder, PortSpec, SandboxClientPort, SandboxCreateArgs};

mod common;

use common::ALPINE;

/// What a process in `sandbox` hears back from the client's port `port` after saying `hi`.
fn ask(sandbox: &virtx::Sandbox, port: u16) -> (Vec<String>, i32) {
    let ask = format!("echo hi | nc -w 5 10.0.2.2 {port}");
    common::run(sandbox, &["sh", "-c", &ask])
}

/// What `ask` hears once it hears it, for 30 seconds: a port let go of a moment ago may still
/// hand a connection to the client that let it go.
fn answered(sandbox: &virtx::Sandbox, port: u16) -> Vec<String> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let (lines, code) = ask(sandbox, port);
        if code == 0 {
            return lines;
        }
        assert!(Instant::now() < deadline, "not reached: {lines:?}");
    }
}

/// A listener on this machine's loopback that answers each line with `back` and the line.
fn listening() -> (u16, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let answering = thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut line = String::new();
            if BufReader::new(&stream).read_line(&mut line).is_ok() {
                let _ = write!(stream, "back {line}");
            }
        }
    });
    (port, answering)
}

/// Over stdio the server is on this machine, and reaches the port's target itself, with no
/// network, while this client takes it, and not before or after; it ends once the sandbox is
/// removed.
#[test]
fn a_client_port_over_stdio_is_reached_while_it_is_taken() {
    let client = common::client();
    common::alpine(&client);
    let (port, _answering) = listening();
    let args = SandboxCreateArgs::new(ALPINE)
        .network(false)
        .client_port(port);
    let sandbox = client.create_sandbox(&args).wait().unwrap();
    let ports: Vec<_> = (sandbox.info().client_ports.iter())
        .map(|p| p.port)
        .collect();
    assert_eq!(ports, [port]);
    let (lines, code) = ask(&sandbox, port);
    assert_ne!(code, 0, "reached a port no one takes: {lines:?}");

    let taken = ClientPortForwarder::open(&sandbox, port, None)
        .wait()
        .unwrap();
    assert_eq!(ask(&sandbox, port), (vec!["back hi".to_owned()], 0));
    let undeclared = ClientPortForwarder::open(&sandbox, port + 1, None).wait();
    assert_eq!(undeclared.err().unwrap().status(), Some(404));
    drop(taken);
    let deadline = Instant::now() + Duration::from_secs(30);
    while ask(&sandbox, port).1 == 0 {
        assert!(Instant::now() < deadline, "still taken once let go of");
    }

    // A target given in place of the port's own is reached, `::1` among them.
    let listener = TcpListener::bind("[::1]:0").unwrap();
    let elsewhere = listener.local_addr().unwrap().port();
    let _answering = thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let _ = write!(stream, "from ::1");
        }
    });
    let target = format!("[::1]:{elsewhere}");
    let taken = ClientPortForwarder::open(&sandbox, port, Some(&target))
        .wait()
        .unwrap();
    assert_eq!(answered(&sandbox, port), ["from ::1"]);
    sandbox.remove().wait().unwrap();
    assert_eq!(taken.wait_closed().status(), Some(404));
}

/// Over HTTP a client port is reached, with no network, while this client takes it, and not
/// before or after; it ends once the sandbox is removed.
#[test]
fn a_client_port_over_http_is_reached_while_it_is_taken() {
    let client = common::tcp_client();
    common::alpine(&client);
    let (port, _answering) = listening();
    let args = SandboxCreateArgs::new(ALPINE)
        .network(false)
        .client_port(PortSpec::new(port).label("answers back"));
    let sandbox = client.create_sandbox(&args).wait().unwrap();
    let labelled = SandboxClientPort {
        port,
        label: Some("answers back".into()),
    };
    assert_eq!(sandbox.info().client_ports, std::slice::from_ref(&labelled));
    let got = client.get_sandbox(sandbox.id()).wait().unwrap();
    assert_eq!(got.info().client_ports, [labelled]);
    let (lines, code) = ask(&sandbox, port);
    assert_ne!(code, 0, "reached a port no one takes: {lines:?}");

    let taken = ClientPortForwarder::open(&sandbox, port, None)
        .wait()
        .unwrap();
    assert_eq!(taken.port(), port);
    assert_eq!(ask(&sandbox, port), (vec!["back hi".to_owned()], 0));
    let undeclared = ClientPortForwarder::open(&sandbox, port + 1, None).wait();
    assert_eq!(undeclared.err().unwrap().status(), Some(404));

    // Let go of, it is no one's again, once the server has seen its WebSocket close.
    drop(taken);
    let deadline = Instant::now() + Duration::from_secs(30);
    while ask(&sandbox, port).1 == 0 {
        assert!(Instant::now() < deadline, "still taken once let go of");
    }

    let taken = ClientPortForwarder::open(&sandbox, port, None)
        .wait()
        .unwrap();
    assert_eq!(answered(&sandbox, port), ["back hi"]);
    // Connections made at once, each on a WebSocket of its own, wait for the next.
    let at_once = format!(
        "for i in 1 2 3 4 5 6 7 8; do (echo $i | nc -w 5 10.0.2.2 {port} > /tmp/$i) & done; wait; \
         cat /tmp/1 /tmp/2 /tmp/3 /tmp/4 /tmp/5 /tmp/6 /tmp/7 /tmp/8"
    );
    let (lines, code) = common::run(&sandbox, &["sh", "-c", &at_once]);
    assert_eq!((lines.len(), code), (8, 0), "{lines:?}");
    sandbox.remove().wait().unwrap();
    assert_eq!(taken.wait_closed().status(), Some(404));
}

/// A connection whose target on this machine is not reached is refused, as one to a port
/// nothing listens on is; and a target given in place of the port's own is reached.
#[test]
fn a_client_port_s_connection_reaches_its_target_or_is_refused() {
    let client = common::tcp_client();
    common::alpine(&client);
    // Nothing listens there once this is dropped.
    let closed = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = closed.local_addr().unwrap().port();
    drop(closed);
    let args = SandboxCreateArgs::new(ALPINE).client_port(port);
    let sandbox = client.create_sandbox(&args).wait().unwrap();

    let taken = ClientPortForwarder::open(&sandbox, port, None)
        .wait()
        .unwrap();
    let (lines, code) = ask(&sandbox, port);
    assert_ne!(code, 0, "reached a target nothing listens on: {lines:?}");
    drop(taken);

    let (elsewhere, _answering) = listening();
    let target = format!("127.0.0.1:{elsewhere}");
    let _taken = ClientPortForwarder::open(&sandbox, port, Some(&target))
        .wait()
        .unwrap();
    assert_eq!(answered(&sandbox, port), ["back hi"]);
    sandbox.remove().wait().unwrap();
}
