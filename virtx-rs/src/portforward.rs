//! A sandbox's port, forwarded to this machine's loopback: a `VirtxPortForwarder` held.

use std::net::SocketAddr;

use serde::{Deserialize, Serialize};

use crate::{
    Client, Error, SandboxPort,
    call::take,
    library::{Api, Opaque, VirtxBytes},
};

/// A sandbox's port at an address on this machine's loopback, for as long as this is held.
///
/// It runs on its client's session, so the client's last drop stops it. Over stdio the server is
/// on this machine and its own address is the forwarder's; over HTTP each connection to the
/// forwarder is carried to the port in a WebSocket. A connection that reaches nothing in the
/// sandbox -- its WebSocket refused, or closed before the port sent a byte, as it is when
/// nothing listens there -- is closed, and why is [`last_error`](PortForwarder::last_error),
/// and logged as a warning under the target `virtx`.
pub struct PortForwarder {
    api: &'static Api,
    raw: *mut Opaque,
    address: SocketAddr,
}

// A `VirtxPortForwarder` is read and freed from any thread.
unsafe impl Send for PortForwarder {}
unsafe impl Sync for PortForwarder {}

/// What a forwarder is opened for: a port as a sandbox answered it, whose number then names it
/// in what a connection that fails says, or the `url` answered for it alone.
pub trait ForwardedPort {
    /// Where the server reaches it, as [`SandboxPort::url`].
    fn url(&self) -> &str;

    /// Its number in the sandbox, where it is known.
    fn port(&self) -> Option<u16> {
        None
    }
}

impl ForwardedPort for str {
    fn url(&self) -> &str {
        self
    }
}

impl ForwardedPort for String {
    fn url(&self) -> &str {
        self
    }
}

impl ForwardedPort for SandboxPort {
    fn url(&self) -> &str {
        &self.url
    }

    fn port(&self) -> Option<u16> {
        Some(self.port)
    }
}

impl<T: ForwardedPort + ?Sized> ForwardedPort for &T {
    fn url(&self) -> &str {
        (**self).url()
    }

    fn port(&self) -> Option<u16> {
        (**self).port()
    }
}

#[derive(Serialize)]
struct Args<'a> {
    url: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    port: Option<u16>,
}

#[derive(Deserialize)]
struct Output {
    address: SocketAddr,
}

impl PortForwarder {
    /// Forward `port`, as [`SandboxInfo::ports`](crate::SandboxInfo) answers one, or the
    /// [`url`](SandboxPort::url) answered for it, over `client`'s session. Blocks until it is
    /// open, which takes no request of the server.
    pub fn open(client: &Client, port: impl ForwardedPort) -> Result<PortForwarder, Error> {
        let api = client.api;
        let args = serde_json::to_vec(&Args {
            url: port.url(),
            port: port.port(),
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

    /// Why the last connection to the forwarder that reached nothing in the sandbox failed, its
    /// message naming the port; `None` while none has. [`Error::Status`] where the server
    /// refused the connection's WebSocket. A server on this machine is reached at its own
    /// address, not through the forwarder, so it has none.
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
