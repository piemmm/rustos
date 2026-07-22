//! Shared wire topology for the QEMU netstack verticals
//! (`plans/NETWORK.md` N4e).
//!
//! One emulated Ethernet link joins two `lib/net` stacks: the *guest*
//! (the two-process production-boot vertical's autoloaded virtio-net
//! driver serving the `tairix-netstack` service) and the *peer* (a
//! host-side `Stack` the harness runs over the QEMU dgram netdev, one
//! raw frame per datagram). Both ends configure themselves from the
//! constants here, so the addresses and identifiers can never drift
//! between the two builds.
//!
//! # Choreography (deterministic, no wall-clock races)
//!
//! The guest has no admin-assigned IPv4: it forms its IPv6 link-local
//! address from its device MAC ([`GUEST_MAC`], modified EUI-64). The
//! peer therefore addresses the guest by that MAC-derived link-local.
//!
//! 1. The peer resolves the guest itself — a Neighbour Solicitation for
//!    the guest's link-local address — and pings it with [`PEER_ECHO_ID`]
//!    / [`PEER_ECHO_PAYLOAD`], retrying until the reply arrives. The guest
//!    answers NS and the echo, so neighbour resolution and the echo path
//!    are proven both ways.
//! 2. The guest's service answers the peer's inbound request; its
//!    `INBOUND_ECHO_SERVED` witness gates the guest's exit, so it never
//!    exits before a frame has crossed the two-process boundary and been
//!    answered.
//!
//! The harness additionally requires the peer thread to report that its
//! own campaign completed, so neither side can pass alone.

#![no_std]
#![forbid(unsafe_code)]

use core::net::Ipv6Addr;

/// Peer IPv6 interface identifier.
pub const PEER_IID: [u8; 8] = [0, 0, 0, 0, 0, 0, 0, 0x02];

/// Peer MAC address (locally administered; the guest's MAC is the
/// device's own report).
pub const PEER_MAC: [u8; 6] = [0x02, 0x52, 0x4F, 0x53, 0x00, 0x02];

/// Fixed MAC pinned on the two-process autoload vertical's QEMU
/// virtio-net device. The guest forms its IPv6 link-local address from
/// this *device* MAC (modified EUI-64), with no admin-assigned IPv4, so
/// the host peer must know the MAC ahead of the run to address the guest.
pub const GUEST_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x00, 0x00, 0x15];

/// [`GUEST_MAC`] rendered as the QEMU `mac=` device-string value.
pub const GUEST_MAC_STR: &str = "52:54:00:00:00:15";

/// Echo identifier of the peer's inbound campaign; the guest's engine
/// mirrors it back and the peer verifies the reflection.
pub const PEER_ECHO_ID: u16 = 0x5EED;

/// Payload of the peer's inbound campaign pings; the guest's engine
/// mirrors it back and the peer verifies the reflection.
pub const PEER_ECHO_PAYLOAD: &[u8] = b"tairix-netstack-peer";

/// The link-local address formed from an interface identifier
/// (`fe80::/64` + IID) — the one derivation both ends use.
#[must_use]
pub const fn link_local(iid: [u8; 8]) -> Ipv6Addr {
    // `fe80::/64` in the high half, the interface identifier in the low
    // half. Built from `u16` segments so construction stays `const` on
    // the pinned MSRV (`Ipv6Addr::from_octets` is a newer const API).
    Ipv6Addr::new(
        0xFE80,
        0,
        0,
        0,
        ((iid[0] as u16) << 8) | iid[1] as u16,
        ((iid[2] as u16) << 8) | iid[3] as u16,
        ((iid[4] as u16) << 8) | iid[5] as u16,
        ((iid[6] as u16) << 8) | iid[7] as u16,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_local_derivation_places_the_iid_in_the_low_half() {
        let addr = link_local(PEER_IID);
        assert_eq!(addr.octets()[..2], [0xFE, 0x80]);
        assert_eq!(addr.octets()[8..], PEER_IID);
    }

    #[test]
    fn guest_mac_string_matches_the_octets() {
        // Parse the QEMU `mac=` string back into octets with core-only
        // arithmetic (the crate is `no_std`, no `alloc`) and confirm it
        // reproduces `GUEST_MAC`, so the two forms cannot drift.
        let mut parsed = [0u8; 6];
        let mut count = 0usize;
        for (slot, field) in parsed.iter_mut().zip(GUEST_MAC_STR.split(':')) {
            *slot = u8::from_str_radix(field, 16).expect("hex octet");
            count += 1;
        }
        assert_eq!(count, 6, "the string has six colon-separated octets");
        assert_eq!(parsed, GUEST_MAC);
    }
}
