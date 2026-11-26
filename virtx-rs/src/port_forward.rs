//! A sandbox's port, forwarded to this machine's loopback: a `VirtxPortForwarder` held.

use std::net::SocketAddr;

use serde::{Deserialize, Serialize};

use crate::{
    Error, Sandbox,
    call::take,
    library::{Api, Opaque, VirtxBytes},
};

/// A sandbox's port at an address on this machine's loopback, for as long as this is held.
///
/// It runs on its client's session, so the client's last drop stops it. Over stdio the server is
/// on this machine and its own address is the forwarder's; over HTTP each connection to the
/// forwarder is carried to the port in a WebSocket. A connection whose WebSocket is refused, or
/// closed before the port sent a byte, is closed, and what was seen is
/// [`last_error`](PortForwarder::last_error), and logged as a warning under the target `virtx`.
pub struct PortForwarder {
    api: &'static Api,
    raw: *mut Opaque,
    address: SocketAddr,
}

// A `VirtxPortForwarder` is read and freed from any thread.
unsafe impl Send for PortForwarder {}
unsafe impl Sync for PortForwarder {}

#[derive(Serialize)]
struct Args<'a> {
    sandbox: &'a str,
    port: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<&'a str>,
}

#[derive(Deserialize)]
struct Output {
    address: SocketAddr,
}

impl PortForwarder {
    /// Forward the port `port` of `sandbox`, one it was made with, over its client's session.
    /// Blocks until it is open, which takes no request of the server. Over stdio the server
    /// answered the port's loopback address with the sandbox, and it is reached there.
    pub fn open(sandbox: &Sandbox, port: u16) -> Result<PortForwarder, Error> {
        let client = sandbox.client();
        let api = client.api;
        let url = (sandbox.info().ports.iter())
            .find(|opened| opened.port == port)
            .and_then(|opened| opened.url.as_deref());
        let args = serde_json::to_vec(&Args {
            sandbox: sandbox.id(),
            port,
            url,
        })
        .map_err(|e| Error::Json(e.to_string()))?;
        let mut raw = std::ptr::null_mut();
        let mut out = VirtxBytes::empty();
        let status = unsafe {
            (api.virtx_port_forwarder_open)(
                client.raw,
                args.as_ptr(),
                args.len(),
                &mut raw,
                &mut out,
            )
        };
        let Output { address } = unsafe { take(api, status, out) }?;
        Ok(PortForwarder { api, raw, address })
    }

    /// Where the port is reached: on this machine's loopback.
    pub fn address(&self) -> SocketAddr {
        self.address
    }

    /// Why the last connection to the forwarder that failed did, its message naming the port,
    /// with the server's reason for closing it if it gave one; `None` while none has, and again
    /// once a later connection's port sends a byte. [`Error::Status`] where the server refused
    /// the connection's WebSocket. A server on this machine is reached at its own address, not
    /// through the forwarder, so it has none.
    pub fn last_error(&self) -> Option<Error> {
        let mut out = VirtxBytes::empty();
        let status = unsafe { (self.api.virtx_port_forwarder_last_error)(self.raw, &mut out) };
        unsafe { take::<serde_json::Value>(self.api, status, out) }.err()
    }
}

impl Drop for PortForwarder {
    /// Stops the forwarding.
    fn drop(&mut self) {
        unsafe { (self.api.virtx_port_forwarder_free)(self.raw) };
    }
}
