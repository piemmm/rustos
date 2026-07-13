//! Deterministic fuzz harness for the `lib/net` wire codecs.
//!
//! Every decoder in this crate parses bytes that arrived from a possibly
//! hostile peer over the link layer, so each one is driven by a fuzz
//! harness. A deterministic, per-run-seeded LCG generates pseudo-random
//! inputs and asserts the two invariants every codec must uphold no
//! matter what bits a peer crafts:
//!
//! 1. Decoding never panics for any input.
//! 2. An accepted decode round-trips through its matching encoder — the
//!    bytes a parser accepts are exactly the bytes its writer produces.
//!
//! A structured bit-flip sweep additionally walks the boundary between
//! accepted and rejected around a well-formed ARP-over-Ethernet frame.
//!
//! ## Wall-clock budget
//!
//! A plain `cargo test` runs the [`SMOKE_ITERATIONS`] sweep once from a
//! fresh, logged seed so the suite stays fast. When `cargo xtask fuzz
//! --soak` exports `RUSTOS_FUZZ_BUDGET_SECS`, the PRNG-driven harness
//! keeps drawing fresh inputs from the *same continuing* stream until
//! the deadline elapses. The seed is logged at the start, so a
//! fresh-seed crash stays reproducible via `RUSTOS_FUZZ_SEED`. The
//! structured bit-flip harness is an exhaustive boundary sweep, not a
//! random one, so it runs once regardless of the budget.

use rustos_abi::driver::net::MacAddress;
use rustos_net::arp::{self, ArpPacket};
use rustos_net::eth::{self, EthernetFrame};
use rustos_net::icmp::IcmpEcho;
use rustos_net::ipv4::{self, Ipv4Header};
use rustos_net::Ipv4Addr;

/// Fixed-iteration sweep run once by a plain `cargo test` (no budget set).
const SMOKE_ITERATIONS: u64 = 100_000;

/// Drive every codec in the crate on `bytes`.
///
/// The contract is "must not panic for any input"; an accepted decode is
/// additionally required to round-trip through its matching encoder.
fn exercise(bytes: &[u8]) {
    exercise_ethernet(bytes);
    exercise_arp(bytes);
    exercise_ipv4(bytes);
    exercise_icmp(bytes);
}

fn exercise_ethernet(bytes: &[u8]) {
    if let Some(frame) = EthernetFrame::parse(bytes) {
        // Re-lay the header and payload, then re-parse: the fields and
        // borrowed payload must survive the round trip exactly.
        let mut out = vec![0u8; eth::ETHERNET_HEADER_LEN + frame.payload.len()];
        let header_len =
            eth::write_header(&mut out, frame.destination, frame.source, frame.ethertype)
                .expect("a parsed header must re-encode");
        out[header_len..].copy_from_slice(frame.payload);
        let redecoded =
            EthernetFrame::parse(&out).expect("round-trip of an accepted frame must parse");
        assert_eq!(frame, redecoded);
    }
}

fn exercise_arp(bytes: &[u8]) {
    if let Some(packet) = ArpPacket::parse(bytes) {
        let mut out = [0u8; arp::ARP_PACKET_LEN];
        packet
            .write(&mut out)
            .expect("a parsed ARP packet must re-encode");
        let redecoded =
            ArpPacket::parse(&out).expect("round-trip of an accepted packet must parse");
        assert_eq!(packet, redecoded);
    }
}

fn exercise_ipv4(bytes: &[u8]) {
    if let Some((header, payload)) = Ipv4Header::parse(bytes) {
        let mut out = vec![0u8; ipv4::IPV4_HEADER_LEN + payload.len()];
        let header_len = header
            .write(&mut out, payload.len())
            .expect("a parsed IPv4 header must re-encode");
        out[header_len..].copy_from_slice(payload);
        let (redecoded, redecoded_payload) =
            Ipv4Header::parse(&out).expect("round-trip of an accepted header must parse");
        assert_eq!(header, redecoded);
        assert_eq!(payload, redecoded_payload);
    }
}

fn exercise_icmp(bytes: &[u8]) {
    if let Some(echo) = IcmpEcho::parse(bytes) {
        let mut out = vec![0u8; echo.wire_len()];
        echo.write(&mut out).expect("a parsed echo must re-encode");
        let redecoded = IcmpEcho::parse(&out).expect("round-trip of an accepted echo must parse");
        assert_eq!(echo, redecoded);
    }
}

/// Lehmer-style LCG — deterministic, no allocator. Identical to the
/// generator in `lib/abi/tests/fuzz_decode.rs` so the two harnesses share
/// one reproducible-failure story.
struct Lcg(u64);

impl Lcg {
    fn new(seed: u64) -> Self {
        Self(if seed == 0 {
            0x9E37_79B9_7F4A_7C15
        } else {
            seed
        })
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        self.0
    }

    fn fill(&mut self, buf: &mut [u8]) {
        let mut i = 0;
        while i < buf.len() {
            let word = self.next_u64().to_le_bytes();
            let take = core::cmp::min(8, buf.len() - i);
            buf[i..i + take].copy_from_slice(&word[..take]);
            i += take;
        }
    }
}

#[test]
fn random_inputs_never_panic() {
    let mut rng = Lcg::new(rustos_fuzzseed::start(
        "random_inputs_never_panic",
        rustos_fuzzseed::FUZZ_SEED_ENV,
    ));
    let mut buf = [0u8; 256];
    let deadline = rustos_fuzzseed::budget_deadline(rustos_fuzzseed::FUZZ_BUDGET_ENV);
    loop {
        for _ in 0..SMOKE_ITERATIONS {
            let size = ((rng.next_u64() & 0x1FF) as usize) % (buf.len() + 1);
            rng.fill(&mut buf[..size]);
            exercise(&buf[..size]);
        }
        if !rustos_fuzzseed::within_budget(deadline) {
            break;
        }
    }
}

/// Build a valid ARP request wrapped in an Ethernet frame.
fn valid_arp_frame() -> Vec<u8> {
    let request = ArpPacket {
        operation: arp::OP_REQUEST,
        sender_hardware: MacAddress([0x02, 0xCA, 0xFE, 0xBA, 0xBE, 0x01]),
        sender_protocol: Ipv4Addr::new(10, 0, 2, 2),
        target_hardware: MacAddress([0; 6]),
        target_protocol: Ipv4Addr::new(10, 0, 2, 15),
    };
    let mut frame = vec![0u8; eth::ETHERNET_HEADER_LEN + arp::ARP_PACKET_LEN];
    let header_len = eth::write_header(
        &mut frame,
        MacAddress([0x52, 0x54, 0x00, 0x12, 0x34, 0x56]),
        request.sender_hardware,
        eth::ETHERTYPE_ARP,
    )
    .expect("header fits");
    request.write(&mut frame[header_len..]).expect("arp fits");
    frame
}

#[test]
fn structured_inputs_with_corrupted_fields_never_panic() {
    // Start from a well-formed ARP-over-Ethernet frame, then bit-flip
    // individual bytes to walk the boundary between accepted and
    // rejected at every layer.
    let mut frame = valid_arp_frame();
    // The pristine frame must decode at both layers, proving the seed
    // is valid.
    let parsed = EthernetFrame::parse(&frame).expect("seed frame parses");
    ArpPacket::parse(parsed.payload).expect("seed ARP parses");

    for byte in 0..frame.len() {
        for bit in 0..8u32 {
            frame[byte] ^= 1 << bit;
            exercise(&frame);
            frame[byte] ^= 1 << bit;
        }
    }
}
