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

// --- TCP stream vertical (N5c) -----------------------------------------
//
// The stream vertical reuses the same emulated link and the same
// IPv6-link-local addressing the ICMP vertical proves (the guest forms its
// link-local from `GUEST_MAC`, the peer from `PEER_IID`), so no address
// configuration or new address family is invented for it. On top of that
// link the guest runs a TCP **client** command (`tcpecho`) that connects to
// a passive TCP **echo** server the host peer hosts, streams a fixed,
// deterministic byte run to it, and verifies the server echoes every byte
// back in order. The host peer injects deterministic frame loss so the run
// exercises RFC 9293 retransmission end to end across the two-process
// boundary, not just a clean link.

/// TCP port the host peer's passive echo server listens on, and the port
/// the guest `tcpecho` client connects to. A fixed, unprivileged value so
/// the two builds cannot drift.
pub const PEER_TCP_PORT: u16 = 7;

/// TCP port the **guest** `tcpserve` echo server binds and listens on, and
/// the port the host client peer connects to, in the role-swapped listener
/// vertical (`plans/NETWORK.md` N6b-2-β-2). A **well-known (privileged)**
/// port (≤ `SOCKET_PRIVILEGED_PORT_MAX` = 1023): binding it exercises the
/// `netstack` privileged-bind gate (`CAP_NET_BIND_PRIVILEGED`) end to end,
/// so the vertical proves the full privileged listener path, not just an
/// ephemeral one. Fixed so the guest server and the host client cannot
/// drift. It is deliberately distinct from [`PEER_TCP_PORT`] so the two
/// TCP verticals never share a port meaning (and a mis-wired end fails
/// closed on the wrong port rather than accidentally matching).
pub const GUEST_TCP_PORT: u16 = 777;

// The listener vertical binds a well-known (privileged) port to exercise the
// netstack privileged-bind gate, and it must stay distinct from the client
// vertical's port so a cross-wired end fails closed on the wrong port rather
// than matching. Compile-time so a bad edit cannot even build.
const _: () = {
    assert!(
        GUEST_TCP_PORT <= 1023,
        "GUEST_TCP_PORT must be a privileged port"
    );
    assert!(
        GUEST_TCP_PORT != PEER_TCP_PORT,
        "the two TCP verticals must not share a port meaning"
    );
};

/// Number of bytes the `tcpecho` client streams to the peer (and expects
/// echoed back). Large enough to span many maximum-segment-sized segments
/// — so windowing, cumulative/selective acknowledgement, and retransmission
/// under the peer's injected loss are all exercised — while staying quick
/// under TCG emulation. Both ends derive their transfer length from this one
/// constant, so a client that sends `STREAM_TRANSFER_BYTES` and a server
/// that echoes every received byte agree by construction.
pub const STREAM_TRANSFER_BYTES: usize = 32 * 1024;

/// The deterministic byte at stream offset `index` of the `tcpecho`
/// transfer. A cheap full-period generator (a byte-wise linear congruential
/// step) so the client can produce the outbound run and verify the echoed
/// run **without buffering** the whole transfer — it recomputes the expected
/// byte at each received offset. Being deterministic, a corrupted or
/// reordered echo is caught at the first mismatched offset, and the run
/// stays byte-exactly replayable.
#[must_use]
pub const fn stream_byte(index: usize) -> u8 {
    // `index * 181 + 89` folded to a byte: 181 is odd (coprime with 256),
    // so the low byte cycles through all 256 values over any 256 consecutive
    // offsets — a well-mixed, non-constant pattern with no external state.
    // The low byte (`& 0xFF`) taken without a truncating cast: `to_le_bytes`
    // is const on the pinned toolchain and its first element is the low byte.
    index.wrapping_mul(181).wrapping_add(89).to_le_bytes()[0]
}

/// Fill `buf` with the deterministic stream bytes starting at stream offset
/// `offset`: `buf[i]` is [`stream_byte(offset + i)`](stream_byte).
///
/// The sending end (the `tcpecho` client, or the host client of the listener
/// vertical) produces each outbound chunk with this; the echoing end mirrors
/// whatever it receives, so the echoed run is byte-identical and
/// [`verify_chunk`] re-derives it to check the echo without buffering the
/// whole transfer. Shared here — the one definition both TCP fixtures and
/// both host peers use — so a sender and a verifier can never disagree about
/// a single byte.
pub fn fill_chunk(offset: usize, buf: &mut [u8]) {
    for (i, byte) in buf.iter_mut().enumerate() {
        *byte = stream_byte(offset + i);
    }
}

/// Verify a received chunk against the deterministic stream: `chunk[i]` must
/// equal [`stream_byte(offset + i)`](stream_byte).
///
/// Returns `Ok(())` when every byte matches, or `Err(index)` naming the first
/// mismatched offset *within the chunk* — a corrupted, reordered, or
/// duplicated byte is caught at its first wrong position rather than accepted
/// (fail closed). The one definition every consumer (both fixtures, both host
/// peers) checks with.
///
/// # Errors
///
/// The chunk-relative index of the first byte that does not match the
/// expected deterministic stream value.
pub fn verify_chunk(offset: usize, chunk: &[u8]) -> Result<(), usize> {
    for (i, &byte) in chunk.iter().enumerate() {
        if byte != stream_byte(offset + i) {
            return Err(i);
        }
    }
    Ok(())
}

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

    #[test]
    fn stream_byte_is_deterministic_and_non_constant() {
        // Re-derivation is stable (the client and its own verification agree)
        // and the pattern is not a single repeated value (a constant stream
        // would not catch a stuck or duplicated segment).
        assert_eq!(stream_byte(0), stream_byte(0));
        assert_eq!(stream_byte(1234), stream_byte(1234));
        assert_ne!(stream_byte(0), stream_byte(1));
    }

    #[test]
    fn stream_byte_covers_every_value_over_256_offsets() {
        // 181 is coprime with 256, so any run of 256 consecutive offsets is a
        // permutation of all byte values — a well-mixed run with no gaps.
        let mut seen = [false; 256];
        for index in 0..256usize {
            seen[stream_byte(index) as usize] = true;
        }
        assert!(seen.iter().all(|&hit| hit), "all 256 byte values appear");
    }

    #[test]
    fn fill_then_verify_round_trips_across_a_chunk_boundary() {
        // Fill two adjacent chunks and confirm verify accepts each at its own
        // offset — a sender fills at the send offset and a verifier checks at
        // the receive offset, so the two must agree byte-for-byte.
        let mut a = [0u8; 100];
        let mut b = [0u8; 100];
        fill_chunk(0, &mut a);
        fill_chunk(100, &mut b);
        assert_eq!(verify_chunk(0, &a), Ok(()));
        assert_eq!(verify_chunk(100, &b), Ok(()));
        // A byte from the wrong offset is rejected at that position.
        assert_eq!(verify_chunk(0, &b), Err(0));
    }

    #[test]
    fn verify_catches_a_single_corrupted_byte() {
        let mut chunk = [0u8; 64];
        fill_chunk(500, &mut chunk);
        chunk[37] ^= 0x01;
        assert_eq!(verify_chunk(500, &chunk), Err(37));
    }

    #[test]
    fn verify_catches_a_reordered_chunk() {
        // Swapping two bytes (a reorder) is caught at the first moved byte.
        let mut chunk = [0u8; 16];
        fill_chunk(9, &mut chunk);
        chunk.swap(2, 11);
        assert_eq!(verify_chunk(9, &chunk), Err(2));
    }
}
