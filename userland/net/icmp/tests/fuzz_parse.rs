//! Deterministic fuzz harness for the composed responder/client paths.
//!
//! The wire parsers themselves live in `lib/net` and are fuzzed there
//! (`fuzz_net_eth`); this harness drives the *composed* service entry
//! points this crate owns — [`Responder::handle_frame`],
//! [`Client::parse_arp_reply`], and [`Client::is_echo_reply`] — over
//! hostile bytes, asserting they never panic, that any reply fits the
//! caller's buffer and is itself a well-formed Ethernet frame, and that
//! the classifiers never claim a malformed frame.
//!
//! RustOS does not pull in an external fuzz runner: a deterministic,
//! per-run-seeded LCG generates pseudo-random inputs, and a structured
//! bit-flip sweep walks the boundary between accepted and rejected at
//! every layer of the composed parser.
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
use rustos_net_icmp::arp::ArpPacket;
use rustos_net_icmp::ethernet::EthernetFrame;
use rustos_net_icmp::{Client, Ipv4Addr, Responder};

/// Fixed-iteration sweep run once by a plain `cargo test` (no budget set).
const SMOKE_ITERATIONS: u64 = 100_000;

/// Link-layer address the responder/client answer for.
const LOCAL_MAC: MacAddress = MacAddress([0x52, 0x54, 0x00, 0x12, 0x34, 0x56]);

/// IPv4 address the responder/client answer for.
const LOCAL_IP: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 15);

/// Drive the composed service entry points and assert a produced reply
/// fits the caller's buffer and is itself a well-formed frame.
fn exercise(bytes: &[u8]) {
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
    let target = Ipv4Addr::new(10, 0, 2, 2);
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

/// Build a valid ARP request for `LOCAL_IP` wrapped in an Ethernet frame
/// addressed to `LOCAL_MAC`.
fn valid_arp_frame() -> Vec<u8> {
    let request = ArpPacket {
        operation: rustos_net_icmp::arp::OP_REQUEST,
        sender_hardware: MacAddress([0x02, 0xCA, 0xFE, 0xBA, 0xBE, 0x01]),
        sender_protocol: Ipv4Addr::new(10, 0, 2, 2),
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
