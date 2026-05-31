//! Deterministic fuzz harness for the `userland/net/icmp` wire parsers.
//!
//! Every parser in this crate decodes a byte slice that arrived from a
//! possibly hostile peer over the link layer, so per `AGENTS.md` §19.5 /
//! §19.6 each one is driven by a fuzz harness. This is the `userland/net`
//! protocol-parser harness the §19.6 burn-down (PLAN.md item 5) calls for;
//! it sits alongside the `lib/abi` decoder harness and the syscall
//! dispatcher harness in the `cargo xtask fuzz` target set.
//!
//! RustOS does not pull in an external fuzz runner (`AGENTS.md` §2.12): a
//! deterministic, fixed-seed LCG generates pseudo-random inputs and asserts
//! the two invariants every parser must uphold no matter what bits a peer
//! crafts:
//!
//! 1. Decoding never panics for any input.
//! 2. An accepted decode round-trips through its matching encoder — the
//!    bytes a parser accepts are exactly the bytes its writer produces.
//!
//! [`Responder::handle_frame`], [`Client::parse_arp_reply`], and
//! [`Client::is_echo_reply`] are the composed entry points a live service
//! exposes; the harness drives them too and additionally checks that any
//! reply [`Responder::handle_frame`] emits fits the caller's buffer and is
//! itself a well-formed Ethernet frame.
//!
//! ## Wall-clock budget (`AGENTS.md` §19.6)
//!
//! A plain `cargo test` runs the fixed [`SMOKE_ITERATIONS`] sweep so the
//! suite stays fast and deterministic. When `cargo xtask fuzz` exports
//! `RUSTOS_FUZZ_BUDGET_SECS`, [`budget`] returns a deadline and the
//! PRNG-driven harness keeps drawing fresh inputs from the *same
//! continuing* stream until it elapses — the §19.6 "run each harness for
//! ≥ 60 s" contract. The seed is fixed, so a crash at draw N stays
//! reproducible regardless of how far a given machine got. The structured
//! bit-flip harness is an exhaustive boundary sweep, not a random one, so
//! it runs once regardless of the budget.

use rustos_abi::driver::net::MacAddress;
use rustos_net_icmp::arp::ArpPacket;
use rustos_net_icmp::ethernet::EthernetFrame;
use rustos_net_icmp::icmp::IcmpEcho;
use rustos_net_icmp::ipv4::Ipv4Header;
use rustos_net_icmp::{Client, Ipv4Address, Responder};

/// Fixed-iteration sweep run by a plain `cargo test` (no budget set).
const SMOKE_ITERATIONS: u64 = 100_000;

/// Link-layer address the responder/client answer for.
const LOCAL_MAC: MacAddress = MacAddress([0x52, 0x54, 0x00, 0x12, 0x34, 0x56]);

/// IPv4 address the responder/client answer for.
const LOCAL_IP: Ipv4Address = Ipv4Address([10, 0, 2, 15]);

/// Deadline for the current run, or `None` for the fixed smoke sweep.
///
/// `cargo xtask fuzz` exports `RUSTOS_FUZZ_BUDGET_SECS` (`AGENTS.md`
/// §19.6); a positive value turns the PRNG-driven harness into a
/// wall-clock loop. An unset, empty, zero, or unparsable value preserves
/// the deterministic smoke behaviour.
fn budget() -> Option<std::time::Instant> {
    let secs: u64 = std::env::var("RUSTOS_FUZZ_BUDGET_SECS")
        .ok()?
        .parse()
        .ok()?;
    if secs == 0 {
        return None;
    }
    Some(std::time::Instant::now() + std::time::Duration::from_secs(secs))
}

/// `true` while the wall-clock budget has time left; always `false` for
/// the fixed smoke sweep so the loop body runs exactly once.
fn within_budget(deadline: Option<std::time::Instant>) -> bool {
    matches!(deadline, Some(end) if std::time::Instant::now() < end)
}

/// Drive every parser in the crate on `bytes`.
///
/// The contract is "must not panic for any input"; an accepted decode is
/// additionally required to round-trip through its matching encoder.
fn exercise(bytes: &[u8]) {
    exercise_ethernet(bytes);
    exercise_arp(bytes);
    exercise_ipv4(bytes);
    exercise_icmp(bytes);
    exercise_composed(bytes);
}

fn exercise_ethernet(bytes: &[u8]) {
    if let Some(frame) = EthernetFrame::parse(bytes) {
        // Re-lay the header and payload, then re-parse: the fields and
        // borrowed payload must survive the round trip exactly.
        let mut out =
            vec![0u8; rustos_net_icmp::ethernet::ETHERNET_HEADER_LEN + frame.payload.len()];
        let header_len = rustos_net_icmp::ethernet::write_header(
            &mut out,
            frame.destination,
            frame.source,
            frame.ethertype,
        )
        .expect("a parsed header must re-encode");
        out[header_len..].copy_from_slice(frame.payload);
        let redecoded =
            EthernetFrame::parse(&out).expect("round-trip of an accepted frame must parse");
        assert_eq!(frame, redecoded);
    }
}

fn exercise_arp(bytes: &[u8]) {
    if let Some(packet) = ArpPacket::parse(bytes) {
        let mut out = [0u8; rustos_net_icmp::arp::ARP_PACKET_LEN];
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
        let mut out = vec![0u8; rustos_net_icmp::ipv4::IPV4_HEADER_LEN + payload.len()];
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

/// Drive the composed service entry points and assert a produced reply
/// fits the caller's buffer and is itself a well-formed frame.
fn exercise_composed(bytes: &[u8]) {
    let responder = Responder::new(LOCAL_MAC, LOCAL_IP);
    let mut out = [0u8; 256];
    if let Ok(Some(len)) = responder.handle_frame(bytes, &mut out) {
        assert!(
            len <= out.len(),
            "reply length must fit the supplied buffer"
        );
        EthernetFrame::parse(&out[..len]).expect("an emitted reply must be a valid Ethernet frame");
    }

    // The client's classifiers are pure predicates over hostile bytes;
    // they must never panic and never claim a malformed frame.
    let client = Client::new(LOCAL_MAC, LOCAL_IP);
    let target = Ipv4Address([10, 0, 2, 2]);
    let _ = client.parse_arp_reply(bytes, target);
    let _ = client.is_echo_reply(bytes, target, 0x1234, 0x0001);
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
    let mut rng = Lcg::new(0xC0FF_EE15_F00D_BABE);
    let mut buf = [0u8; 256];
    let deadline = budget();
    loop {
        for _ in 0..SMOKE_ITERATIONS {
            let size = ((rng.next_u64() & 0x1FF) as usize) % (buf.len() + 1);
            rng.fill(&mut buf[..size]);
            exercise(&buf[..size]);
        }
        if !within_budget(deadline) {
            break;
        }
    }
}

/// Build a valid ARP request for `LOCAL_IP` wrapped in an Ethernet frame
/// addressed to `LOCAL_MAC`.
fn valid_arp_frame() -> Vec<u8> {
    let request = ArpPacket {
        operation: rustos_net_icmp::arp::OP_REQUEST,
        sender_hardware: MacAddress([0x02, 0xCA, 0xFE, 0xBA, 0xBE, 0x01]),
        sender_protocol: Ipv4Address([10, 0, 2, 2]),
        target_hardware: MacAddress([0; 6]),
        target_protocol: LOCAL_IP,
    };
    let mut frame = vec![
        0u8;
        rustos_net_icmp::ethernet::ETHERNET_HEADER_LEN
            + rustos_net_icmp::arp::ARP_PACKET_LEN
    ];
    let header_len = rustos_net_icmp::ethernet::write_header(
        &mut frame,
        LOCAL_MAC,
        request.sender_hardware,
        rustos_net_icmp::ethernet::ETHERTYPE_ARP,
    )
    .expect("header fits");
    request.write(&mut frame[header_len..]).expect("arp fits");
    frame
}

#[test]
fn structured_inputs_with_corrupted_fields_never_panic() {
    // Start from a well-formed ARP-over-Ethernet frame, then bit-flip
    // individual bytes to walk the boundary between accepted and rejected
    // at every layer of the composed parser.
    let mut frame = valid_arp_frame();
    // The pristine frame must elicit a reply, proving the seed is valid.
    let responder = Responder::new(LOCAL_MAC, LOCAL_IP);
    let mut out = [0u8; 256];
    assert!(
        matches!(responder.handle_frame(&frame, &mut out), Ok(Some(_))),
        "the un-mutated seed frame must be answered"
    );

    for byte in 0..frame.len() {
        for bit in 0..8u32 {
            frame[byte] ^= 1 << bit;
            exercise(&frame);
            frame[byte] ^= 1 << bit;
        }
    }
}
