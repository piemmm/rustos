//! Shared wire topology for the QEMU netstack verticals
//! (`plans/NETWORK.md` N3c).
//!
//! One emulated Ethernet link joins two `lib/net` stacks: the *guest*
//! (the freestanding vertical's `tairix-netstack` engine pumping a live
//! virtio-net device) and the *peer* (a host-side `Stack` the harness
//! runs over the QEMU dgram netdev, one raw frame per datagram). Both
//! ends configure themselves from the constants here, so the addresses
//! and identifiers can never drift between the two builds.
//!
//! # Choreography (deterministic, no wall-clock races)
//!
//! 1. The peer resolves the guest itself — a fresh ARP request for
//!    [`GUEST_V4`] and a Neighbour Solicitation for the guest's
//!    link-local address — and pings the guest over v4 *and* v6 with
//!    [`PEER_ECHO_ID`], retrying until each reply arrives. This is the
//!    inbound half: the guest answered ARP, NS, and both echoes.
//! 2. The guest pumps until it has *observed* both inbound requests
//!    (the engine's `EchoRequestServed` events carrying
//!    [`PEER_ECHO_ID`]), so it never exits before the peer's campaign
//!    has been answered.
//! 3. The guest then resolves and pings the peer over v4 and v6 with
//!    [`GUEST_ECHO_ID`] + [`GUEST_ECHO_PAYLOAD`] and exits successfully
//!    only after both replies arrive. This is the outbound half:
//!    neighbours resolved both ways, echo answered both ways, both
//!    families.
//!
//! The harness additionally requires the peer thread to report that its
//! own campaign completed, so a guest that fabricates success cannot
//! pass alone.

#![no_std]
#![forbid(unsafe_code)]

use core::net::{Ipv4Addr, Ipv6Addr};

/// Guest interface alias (the `netstack` interface-table name).
pub const IF_NAME: &str = "wan";

/// Shared IPv4 subnet prefix length (RFC 5737 TEST-NET-1, `192.0.2.0/24`).
pub const V4_PREFIX: u8 = 24;

/// Guest IPv4 address.
pub const GUEST_V4: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 15);

/// Peer IPv4 address.
pub const PEER_V4: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 2);

/// Guest IPv6 interface identifier: its link-local address is
/// [`link_local`]`(GUEST_IID)`.
pub const GUEST_IID: [u8; 8] = [0, 0, 0, 0, 0, 0, 0, 0x15];

/// Peer IPv6 interface identifier.
pub const PEER_IID: [u8; 8] = [0, 0, 0, 0, 0, 0, 0, 0x02];

/// Peer MAC address (locally administered; the guest's MAC is the
/// device's own report).
pub const PEER_MAC: [u8; 6] = [0x02, 0x52, 0x4F, 0x53, 0x00, 0x02];

/// Fixed MAC pinned on the two-process autoload vertical's QEMU
/// virtio-net device (`plans/NETWORK.md` N4e-β). That vertical's guest
/// forms its IPv6 link-local address from the *device* MAC (modified
/// EUI-64), with no admin-assigned IPv4, so the host peer must know the
/// MAC ahead of the run to address the guest. The single-process wire
/// verticals instead configure the guest from [`GUEST_IID`] and ignore
/// the device MAC, so this constant is used only by the two-process
/// vertical.
pub const GUEST_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x00, 0x00, 0x15];

/// [`GUEST_MAC`] rendered as the QEMU `mac=` device-string value.
pub const GUEST_MAC_STR: &str = "52:54:00:00:00:15";

/// Echo identifier of the peer's inbound campaign; the guest keys its
/// "peer campaign answered" observation on it.
pub const PEER_ECHO_ID: u16 = 0x5EED;

/// Payload of the peer's inbound campaign pings; the guest's engine
/// mirrors it back and the peer verifies the reflection.
pub const PEER_ECHO_PAYLOAD: &[u8] = b"tairix-netstack-peer";

/// Echo identifier of the guest's outbound pings.
pub const GUEST_ECHO_ID: u16 = 0x1234;

/// Payload of the guest's outbound pings; replies must mirror it.
pub const GUEST_ECHO_PAYLOAD: &[u8] = b"tairix-netstack-vertical";

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
    fn addresses_share_the_test_net_subnet() {
        assert_eq!(GUEST_V4.octets()[..3], PEER_V4.octets()[..3]);
        assert_ne!(GUEST_V4, PEER_V4);
    }

    #[test]
    fn link_local_derivation_places_the_iid_in_the_low_half() {
        let addr = link_local(GUEST_IID);
        assert_eq!(addr.octets()[..2], [0xFE, 0x80]);
        assert_eq!(addr.octets()[8..], GUEST_IID);
    }

    #[test]
    fn identifiers_are_distinct_so_the_phases_cannot_alias() {
        assert_ne!(PEER_ECHO_ID, GUEST_ECHO_ID);
        assert_ne!(GUEST_IID, PEER_IID);
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
