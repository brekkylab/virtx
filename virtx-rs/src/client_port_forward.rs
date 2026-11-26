//! A sandbox's client port, taken by this client: a `VirtxStream` of `virtx_client_port_forwarder_open`
//! held.

use crate::{Error, Events, Holding, Sandbox};

/// A client port of a sandbox's, one it was made with in its
/// [`client_ports`](crate::SandboxCreateArgs::client_ports), taken by this client while this is
/// held: each connection a process in the sandbox makes to `10.0.2.2:{port}` is carried to its
/// target on this machine, and refused when the target is not reached. Dropped, the port is let
/// go of, and the connections it carries are closed. What fails on the way is logged as a
/// warning under the target `virtx`.
pub struct ClientPortForwarder {
    port: u16,
    events: Events<serde_json::Value>,
}

impl ClientPortForwarder {
    /// Take the client port `port` of `sandbox`, one it was made with, its connections carried
    /// to `target`, `host:port`, or `127.0.0.1:{port}` without one, over its client's session:
    /// answered once it is taken. It is reached while this is held, beside any other's that
    /// takes it. Refused `404` for a port the sandbox was not made with.
    pub fn open(
        sandbox: &Sandbox,
        port: u16,
        target: Option<&str>,
    ) -> Holding<ClientPortForwarder> {
        let events = sandbox
            .client()
            .open_client_port(sandbox.id(), port, target);
        Holding::new(events, move |_, events| {
            Ok(ClientPortForwarder { port, events })
        })
    }

    /// The port, as a process in the sandbox reaches it at `10.0.2.2`.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Block until it is no longer taken, and answer why: `404` once the sandbox is gone, and
    /// what else ended it.
    pub fn wait_closed(mut self) -> Error {
        loop {
            match self.events.wait_next() {
                Some(Ok(_)) => continue,
                Some(Err(e)) => return e,
                None => return Error::Call(virtx_core::Source::Transport, "it ended".into()),
            }
        }
    }
}
