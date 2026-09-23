//! The one lease this stack hands out, and the only reason the guest needs no setting up.
//!
//! # Why a server and not an address on a cmdline
//!
//! Because `/etc/resolv.conf` is the thing that actually has to be written, and the guest
//! is the only end that can write it -- it is a file on a filesystem the host never
//! mounts. libkrun's init already knows how: told [`DHCP_ENV`](super::DHCP_ENV), it runs a
//! DHCP client on `eth0`, and it writes the resolver out of the lease it gets. So a
//! server here is not a way of assigning an address -- the address could have been a
//! kernel argument -- it is the only channel to the one file that makes names work.
//!
//! It also brings the interface up, adds the default route, and sets the MTU, none of
//! which an image has anything in it to do. The whole of the guest's networking is this
//! exchange, which is why a session boots straight into a workload rather than into
//! something that has to configure a machine first.
//!
//! # One lease, forever, to one guest
//!
//! There is one client, it is the only thing on this subnet, and it cannot move. So there
//! is no pool, no allocation, no conflict detection and no expiry worth the name: the
//! lease is [`GUEST`](super::GUEST), the term is long enough that a session never renews,
//! and a request for anything else is answered with what there is.
//!
//! # A hundred milliseconds
//!
//! That is the client's timeout, after which it gives up and the guest has no network at
//! all. Nothing here may wait on anything -- a reply is built from constants and returned
//! on the spot, which is also why there is no state to keep between the two messages of
//! an exchange that does not need them.

use std::net::Ipv4Addr;

use super::{GATEWAY, GUEST, MTU, SUBNET_PREFIX};

/// The ports the two ends speak from, which BOOTP fixed and nothing since has moved.
pub(crate) const SERVER_PORT: u16 = 67;
pub(crate) const CLIENT_PORT: u16 = 68;

/// Where the options start, after the BOOTP header and the cookie that says they are
/// there at all.
const OPTIONS_AT: usize = 240;

/// `99.130.83.99` -- what tells a BOOTP reader that what follows is DHCP.
const COOKIE: [u8; 4] = [0x63, 0x82, 0x53, 0x63];

/// A reply, which is a `BOOTREPLY` whatever the request was.
const BOOTREPLY: u8 = 2;

/// Ethernet, and six bytes of it. The only hardware this stack has.
const HTYPE_ETHERNET: u8 = 1;
const HLEN_ETHERNET: u8 = 6;

/// The message types this exchange has in it. The client sends one of the first two and
/// is answered with the third -- see [`reply`].
const DISCOVER: u8 = 1;
const REQUEST: u8 = 3;
const ACK: u8 = 5;

/// How long the lease runs. A week, which is not a term so much as a number large enough
/// that no session lives to renew -- and a renewal this server would answer the same way
/// in any case.
const LEASE_SECS: u32 = 7 * 24 * 60 * 60;

/// Offsets into the BOOTP header of the three fields a reply does not invent.
const XID: std::ops::Range<usize> = 4..8;
const FLAGS: std::ops::Range<usize> = 10..12;
const YIADDR: std::ops::Range<usize> = 16..20;
const SIADDR: std::ops::Range<usize> = 20..24;
const CHADDR: std::ops::Range<usize> = 28..44;

/// The answer to `request`, or `None` if it is not a question this server was asked.
///
/// Both messages of the exchange get the same answer, which is what the client's Rapid
/// Commit option asks for and what a client without it gets anyway: there is one address
/// and it is not going to be offered to anyone else in between, so an offer and a commit
/// are the same statement made twice.
pub(crate) fn reply(request: &[u8]) -> Option<Vec<u8>> {
    if request.len() < OPTIONS_AT || request[OPTIONS_AT - 4..OPTIONS_AT] != COOKIE {
        return None;
    }
    if !matches!(message_type(request), Some(DISCOVER | REQUEST)) {
        return None;
    }

    let mut reply = vec![0u8; OPTIONS_AT];
    reply[0] = BOOTREPLY;
    reply[1] = HTYPE_ETHERNET;
    reply[2] = HLEN_ETHERNET;
    reply[XID].copy_from_slice(&request[XID]);
    reply[FLAGS].copy_from_slice(&request[FLAGS]);
    reply[YIADDR].copy_from_slice(&GUEST.octets());
    reply[SIADDR].copy_from_slice(&GATEWAY.octets());
    reply[CHADDR].copy_from_slice(&request[CHADDR]);
    reply[OPTIONS_AT - 4..].copy_from_slice(&COOKIE);

    option(&mut reply, 53, &[ACK]);
    option(&mut reply, 54, &GATEWAY.octets());
    option(&mut reply, 51, &LEASE_SECS.to_be_bytes());
    option(&mut reply, 1, &netmask().octets());
    option(&mut reply, 3, &GATEWAY.octets());
    // The resolver is this stack itself -- see `super::GATEWAY`. This is the option the
    // guest turns into `/etc/resolv.conf`, and the whole reason for any of this.
    option(&mut reply, 6, &GATEWAY.octets());
    option(&mut reply, 26, &(MTU as u16).to_be_bytes());
    reply.push(255);

    Some(reply)
}

/// The mask [`SUBNET_PREFIX`] states, as the four bytes option 1 carries.
fn netmask() -> Ipv4Addr {
    Ipv4Addr::from(u32::MAX << (32 - SUBNET_PREFIX))
}

/// Option 53, which is what says whether this is a question at all.
fn message_type(packet: &[u8]) -> Option<u8> {
    options(packet)
        .find(|(code, _)| *code == 53)?
        .1
        .first()
        .copied()
}

/// Append one option, which is a code, a length and that many bytes.
fn option(out: &mut Vec<u8>, code: u8, value: &[u8]) {
    out.push(code);
    out.push(value.len() as u8);
    out.extend_from_slice(value);
}

/// The options in a packet, stopping at the end marker or at the first one that runs off
/// the end -- a short option is a malformed packet, and what a malformed packet gets is
/// the options that were whole.
fn options(packet: &[u8]) -> impl Iterator<Item = (u8, &[u8])> {
    let mut rest = packet.get(OPTIONS_AT..).unwrap_or(&[]);

    std::iter::from_fn(move || {
        loop {
            let (&code, tail) = rest.split_first()?;
            match code {
                // The end, and padding, which is a code with no length after it.
                255 => return None,
                0 => rest = tail,
                _ => {
                    let (&len, tail) = tail.split_first()?;
                    let value = tail.get(..len as usize)?;
                    rest = &tail[len as usize..];
                    return Some((code, value));
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A client's packet: the header, the cookie, and one option saying what it is.
    fn request(kind: u8) -> Vec<u8> {
        let mut packet = vec![0u8; OPTIONS_AT];
        packet[0] = 1;
        packet[XID].copy_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
        packet[CHADDR][..6].copy_from_slice(&super::super::GUEST_MAC);
        packet[OPTIONS_AT - 4..].copy_from_slice(&COOKIE);
        packet.extend_from_slice(&[53, 1, kind, 255]);
        packet
    }

    fn value(packet: &[u8], code: u8) -> Vec<u8> {
        options(packet)
            .find(|(c, _)| *c == code)
            .unwrap_or_else(|| panic!("option {code} is not in the reply"))
            .1
            .to_vec()
    }

    /// What the guest turns into an address, a route, a resolver and an MTU -- the whole
    /// of its networking, so every one of them has to be in one reply.
    #[test]
    fn a_lease_carries_everything_the_guest_has_no_other_way_to_learn() {
        let reply = reply(&request(DISCOVER)).expect("a discover is answered");

        assert_eq!(reply[0], BOOTREPLY);
        assert_eq!(reply[YIADDR], GUEST.octets());
        assert_eq!(value(&reply, 53), [ACK]);
        assert_eq!(value(&reply, 1), [255, 255, 255, 0]);
        assert_eq!(value(&reply, 3), GATEWAY.octets());
        assert_eq!(value(&reply, 6), GATEWAY.octets());
        assert_eq!(value(&reply, 26), (MTU as u16).to_be_bytes());
    }

    /// The client matches a reply to its request by the transaction id and takes the
    /// hardware address as its own, so both are the request's rather than this end's.
    #[test]
    fn a_reply_is_addressed_to_the_request_that_asked_for_it() {
        let request = request(REQUEST);
        let reply = reply(&request).expect("a request is answered");

        assert_eq!(reply[XID], request[XID]);
        assert_eq!(reply[CHADDR], request[CHADDR]);
    }

    /// Rapid Commit, and what a client without it does: the second message of the
    /// exchange is answered exactly as the first, because there is nothing to commit.
    #[test]
    fn both_messages_of_the_exchange_get_the_same_answer() {
        assert_eq!(reply(&request(DISCOVER)), reply(&request(REQUEST)));
    }

    /// Nothing else on port 67 is this server's to answer, and a packet it does not
    /// understand is one it must not reply to -- a reply would be an address handed to
    /// something that did not ask for one.
    #[test]
    fn what_is_not_a_question_gets_no_answer() {
        assert!(reply(&[]).is_none(), "an empty packet");
        assert!(reply(&vec![0u8; OPTIONS_AT]).is_none(), "no cookie");
        assert!(reply(&request(ACK)).is_none(), "a reply, not a request");
    }

    /// A short option runs off the end of the packet, and reading it would read whatever
    /// is after the buffer. It ends the walk instead.
    #[test]
    fn an_option_that_runs_off_the_end_ends_the_walk() {
        let mut packet = vec![0u8; OPTIONS_AT];
        packet[OPTIONS_AT - 4..].copy_from_slice(&COOKIE);
        packet.extend_from_slice(&[53, 1, DISCOVER, 6, 4, 1, 2]);

        assert_eq!(options(&packet).count(), 1, "the truncated option was read");
    }
}
