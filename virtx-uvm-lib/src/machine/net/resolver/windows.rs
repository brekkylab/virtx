//! What a Windows host has: no file, and an API that walks the adapters instead.
//!
//! # Why `GetAdaptersAddresses` and not the registry
//!
//! Because what a session wants is the resolvers that are *in effect*, and the registry
//! holds what was configured. An adapter that took its resolvers from DHCP, a VPN that
//! brought its own, an interface that is down -- all three are differences between the
//! two, and the adapter list is the side that already accounts for them.
//!
//! # The order, which is the metric's
//!
//! Adapters come back in the order Windows would use them, so the resolvers do too: the
//! first one this returns is the first one the host itself would ask. Only adapters that
//! are up are looked at, since a resolver on an interface that is down is one nothing on
//! this machine would reach either.

use std::net::Ipv4Addr;

use windows_sys::Win32::Foundation::{ERROR_BUFFER_OVERFLOW, ERROR_SUCCESS};
use windows_sys::Win32::NetworkManagement::IpHelper::{
    GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_FRIENDLY_NAME, GAA_FLAG_SKIP_MULTICAST,
    GAA_FLAG_SKIP_UNICAST, GET_ADAPTERS_ADDRESSES_FLAGS, GetAdaptersAddresses,
    IP_ADAPTER_ADDRESSES_LH,
};
// An adapter's operational status is NDIS's rather than the IP helper's, which is also
// why the call itself is behind that feature -- see this crate's manifest.
use windows_sys::Win32::NetworkManagement::Ndis::IfOperStatusUp;
use windows_sys::Win32::Networking::WinSock::{AF_INET, AF_UNSPEC, SOCKADDR_IN};

/// What the walk asks for: the resolvers, and none of the several other lists an adapter
/// carries. Everything skipped here is a list the API would otherwise build and this
/// would then walk past.
const WANTED: GET_ADAPTERS_ADDRESSES_FLAGS = GAA_FLAG_SKIP_UNICAST
    | GAA_FLAG_SKIP_ANYCAST
    | GAA_FLAG_SKIP_MULTICAST
    | GAA_FLAG_SKIP_FRIENDLY_NAME;

/// Where the first call starts. The API states the size it actually wants when this is
/// too small, so the number is only a guess at how often that second call is needed.
const FIRST_GUESS: u32 = 16 * 1024;

/// How many times the buffer may be resized before giving up. The size comes back from
/// the API rather than being doubled blindly, so more than one retry means the adapter
/// list is changing underneath the call -- which it can, and which is not worth chasing.
const ATTEMPTS: usize = 4;

/// The host's resolvers, in the order Windows would use them.
pub(crate) fn resolvers() -> Vec<Ipv4Addr> {
    let Some(buffer) = adapters() else {
        return Vec::new();
    };

    let mut resolvers = Vec::new();
    // SAFETY: `adapters` returned a buffer the API filled, so the head of it is an
    // `IP_ADAPTER_ADDRESSES_LH` and every `Next` is either another one or null. Nothing
    // in the walk outlives `buffer`.
    unsafe {
        let mut adapter = buffer.as_ptr() as *const IP_ADAPTER_ADDRESSES_LH;
        while !adapter.is_null() {
            if (*adapter).OperStatus == IfOperStatusUp {
                let mut server = (*adapter).FirstDnsServerAddress;
                while !server.is_null() {
                    if let Some(address) = ipv4((*server).Address.lpSockaddr.cast()) {
                        resolvers.push(address);
                    }
                    server = (*server).Next;
                }
            }
            adapter = (*adapter).Next;
        }
    }

    // Two adapters commonly carry the same resolver, and they need not be next to each
    // other in the list -- so this is not `dedup`, which only looks at neighbours. The
    // order is the host's own preference and is kept.
    let mut seen = Vec::new();
    resolvers.retain(|address| match seen.contains(address) {
        true => false,
        false => {
            seen.push(*address);
            true
        }
    });
    resolvers
}

/// The adapter list, as the bytes the API wrote it into.
fn adapters() -> Option<Vec<u8>> {
    let mut size = FIRST_GUESS;

    for _ in 0..ATTEMPTS {
        let mut buffer = vec![0u8; size as usize];
        // SAFETY: a buffer of `size` bytes and the length being handed in by pointer, as
        // the API asks. It writes nothing on a failure other than the size it wanted.
        let result = unsafe {
            GetAdaptersAddresses(
                // Both families, because an adapter's resolvers are listed on the adapter
                // rather than per family -- a v4 resolver reached over a v6-configured
                // adapter is still a v4 resolver.
                AF_UNSPEC as u32,
                WANTED,
                std::ptr::null_mut(),
                buffer.as_mut_ptr().cast(),
                &mut size,
            )
        };

        match result {
            ERROR_SUCCESS => return Some(buffer),
            // `size` now holds what it actually wants, so the next round is the last.
            ERROR_BUFFER_OVERFLOW => continue,
            _ => return None,
        }
    }
    None
}

/// One `SOCKADDR`, if it is an IPv4 one. A v6 resolver is passed over rather than
/// refused -- see the module doc above this one.
///
/// # Safety
///
/// `address` is either null or a `SOCKADDR` the API wrote, which is what makes reading
/// its family and then its v4 body sound.
unsafe fn ipv4(address: *const SOCKADDR_IN) -> Option<Ipv4Addr> {
    if address.is_null() {
        return None;
    }
    unsafe {
        if (*address).sin_family != AF_INET {
            return None;
        }
        Some(Ipv4Addr::from(
            (*address).sin_addr.S_un.S_addr.to_ne_bytes(),
        ))
    }
}
