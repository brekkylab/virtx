//! A sandbox's port forwarded by this client, on either side of the sandbox: a
//! `VirtxPortForwarder` held.

use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use virtx_core::PortSide;

use crate::{
    Call, Error, Sandbox,
    call::take,
    library::{Api, Opaque, VirtxBytes},
};

/// A sandbox's port, on either side of it, forwarded for as long as this is held.
///
/// A port in the sandbox is reached at the forwarder's [`local`](PortForwarder::local) end, an
/// address on this machine. A port on local, one the sandbox was made with on
/// [`PortSide::Local`], is reached by the sandbox's processes at `10.0.2.2:{port}`, each of
/// their connections carried to the forwarder's local end, its target on this machine, and
/// refused when that is not reached.
///
/// It runs on its client's session, so the client's last drop stops it. A connection that fails
/// on the way is closed, and what was seen is [`last_error`](PortForwarder::last_error), and
/// logged as a warning under the target `virtx`.
pub struct PortForwarder {
    raw: Raw,
    port: u16,
    on: PortSide,
    local: String,
}

/// A `VirtxPortForwarder`, freed when it drops.
struct Raw {
    api: &'static Api,
    raw: *mut Opaque,
}

// A `VirtxPortForwarder` is read and freed from any thread.
unsafe impl Send for Raw {}
unsafe impl Sync for Raw {}

impl Drop for Raw {
    /// Stops the forwarding.
    fn drop(&mut self) {
        unsafe { (self.api.virtx_port_forwarder_free)(self.raw) };
    }
}

#[derive(Serialize)]
struct Args<'a> {
    sandbox: &'a str,
    port: u16,
    on: PortSide,
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    local: Option<&'a str>,
}

#[derive(Deserialize)]
struct Opened {
    local: String,
}

impl PortForwarder {
    /// Forward the port `port` of `sandbox`, one it was made with on the side `on`, over its
    /// client's session: answered once it is open. `local`, `host:port`, is its local end: for
    /// a port in the sandbox the address it is reached at, a free port on `127.0.0.1` without
    /// one, or over stdio the address the server answered for the port; for a port on local the
    /// target its connections are carried to, `127.0.0.1:{port}` without one. A port on local
    /// the sandbox was not made with is refused `404`.
    pub fn open(
        sandbox: &Sandbox,
        port: u16,
        on: PortSide,
        local: Option<&str>,
    ) -> Call<PortForwarder> {
        let client = sandbox.client();
        let api = client.api;
        let url = (sandbox.info().ports.iter())
            .find(|opened| opened.on == on && opened.port == port)
            .and_then(|opened| opened.url.as_deref());
        let args = Args {
            sandbox: sandbox.id(),
            port,
            on,
            url,
            local,
        };
        let args = serde_json::to_vec(&args).expect("the arguments encode");
        let mut raw = std::ptr::null_mut();
        let opening = unsafe {
            (api.virtx_port_forwarder_open)(client.raw, args.as_ptr(), args.len(), &mut raw)
        };
        // Freed with the call when it does not open.
        let held = Mutex::new((!raw.is_null()).then_some(Raw { api, raw }));
        let call: Call<Opened> = Call::new(api, opening);
        call.map(move |Opened { local }| PortForwarder {
            raw: (held.lock().unwrap().take()).expect("opened once"),
            port,
            on,
            local,
        })
    }

    /// The port, on its side.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Which side the port is on.
    pub fn on(&self) -> PortSide {
        self.on
    }

    /// Its local end, as `host:port`: for a port in the sandbox the address it is reached at,
    /// for a port on local the target its connections are carried to.
    pub fn local(&self) -> &str {
        &self.local
    }

    /// Why the last connection through the forwarder that failed did, its message naming the
    /// port, with the server's reason for closing it if it gave one; `None` while none has, and
    /// again once a later connection reaches the port. [`Error::Status`] where the server
    /// refused the connection's WebSocket. A server on this machine reaches what it is itself,
    /// not through the forwarder, so for a port in the sandbox at its own address, and for a
    /// port on local over stdio, it has none.
    pub fn last_error(&self) -> Option<Error> {
        let Raw { api, raw } = self.raw;
        let mut out = VirtxBytes::empty();
        let status = unsafe { (api.virtx_port_forwarder_last_error)(raw, &mut out) };
        unsafe { take::<serde_json::Value>(api, status, out) }.err()
    }

    /// Answers once it has ended, with what ended it as the error: for a port on local, `404`
    /// once its sandbox is gone. A port in the sandbox is forwarded until this drops.
    pub fn closed(&self) -> Call<()> {
        let Raw { api, raw } = self.raw;
        Call::new(api, unsafe { (api.virtx_port_forwarder_closed)(raw) })
    }
}
