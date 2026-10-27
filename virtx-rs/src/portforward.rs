//! A sandbox's port, forwarded to this machine's loopback: a `VirtxPortForwarder` held.

use std::net::SocketAddr;

use serde::{Deserialize, Serialize};

use crate::{
    Client, Error,
    call::take,
    library::{Api, Opaque, VirtxBytes},
};

/// A sandbox's port at an address on this machine's loopback, for as long as this is held.
///
/// It runs on its client's session, so the client's last drop stops it. Over stdio the server is
/// on this machine and its own address is the forwarder's; over HTTP each connection to the
/// forwarder is carried to the port in a WebSocket.
pub struct PortForwarder {
    api: &'static Api,
    raw: *mut Opaque,
    address: SocketAddr,
}

// A `VirtxPortForwarder` is only freed, from whichever thread drops it.
unsafe impl Send for PortForwarder {}
unsafe impl Sync for PortForwarder {}

#[derive(Serialize)]
struct Args<'a> {
    url: &'a str,
}

#[derive(Deserialize)]
struct Output {
    address: SocketAddr,
}

impl PortForwarder {
    /// Forward the port a sandbox answered `url` for, as [`SandboxPort::url`](crate::SandboxPort)
    /// is, over `client`'s session. Blocks until it is open.
    pub fn open(client: &Client, url: impl AsRef<str>) -> Result<PortForwarder, Error> {
        let api = client.api;
        let args = serde_json::to_vec(&Args { url: url.as_ref() })
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
}

impl Drop for PortForwarder {
    /// Stops the forwarding.
    fn drop(&mut self) {
        unsafe { (self.api.virtx_port_forwarder_free)(self.raw) };
    }
}
