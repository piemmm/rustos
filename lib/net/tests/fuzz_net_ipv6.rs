//! Deterministic fuzz harness for the IPv6 codec and the
//! extension-header chain walk.
//!
//! Invariants, for any input bits a peer crafts:
//!
//! 1. Header parsing and the chain walk never panic.
//! 2. An accepted header parse round-trips through the writer.
//! 3. The walk terminates within its fixed chain bound (guaranteed by
//!    construction; exercised here against adversarial chains).
//!
//! Runs the fixed smoke sweep under plain `cargo test`; keeps drawing
//! from the same seeded stream until `RUSTOS_FUZZ_BUDGET_SECS` elapses
//! under `cargo xtask fuzz`.

use rustos_net::ipv6::{self, Ipv6Header};
use rustos_net::Ipv6Addr;

/// Fixed-iteration sweep run once by a plain `cargo test` (no budget set).
const SMOKE_ITERATIONS: u64 = 20_000;

fn exercise_header(bytes: &[u8]) {
    if let Some((header, payload)) = Ipv6Header::parse(bytes) {
        let mut out = vec![0u8; ipv6::IPV6_HEADER_LEN + payload.len()];
        let header_len = header
            .write(&mut out, payload.len())
            .expect("a parsed IPv6 header must re-encode");
        out[header_len..].copy_from_slice(payload);
        let (redecoded, redecoded_payload) =
            Ipv6Header::parse(&out).expect("round-trip of an accepted header must parse");
        assert_eq!(header, redecoded);
        assert_eq!(payload, redecoded_payload);
    }
}

fn exercise_walk(first_header: u8, payload: &[u8]) {
    for multicast in [false, true] {
        // Totality is the invariant: any outcome is fine, panicking or
        // failing to terminate is not.
        let _ = ipv6::walk(first_header, payload, multicast);
    }
}

/// Lehmer-style LCG — deterministic, no allocator. Identical to the
/// generator in the sibling harnesses so failures reproduce one way.
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
            exercise_header(&buf[..size]);
            // Bias the first header toward the extension-header values
            // so the walk's interesting branches are actually hit.
            let first = match rng.next_u64() % 8 {
                0 => ipv6::NEXT_HEADER_HOP_BY_HOP,
                1 => ipv6::NEXT_HEADER_ROUTING,
                2 => ipv6::NEXT_HEADER_FRAGMENT,
                3 => ipv6::NEXT_HEADER_DEST_OPTS,
                4 => ipv6::NEXT_HEADER_NO_NEXT,
                _ => (rng.next_u64() & 0xFF) as u8,
            };
            exercise_walk(first, &buf[..size]);
        }
        if !rustos_fuzzseed::within_budget(deadline) {
            break;
        }
    }
}

/// A valid packet: fixed header, hop-by-hop, destination options,
/// fragment header, then payload bytes.
fn valid_chain_packet() -> Vec<u8> {
    let source = Ipv6Addr::new(0xFE80, 0, 0, 0, 0, 0, 0, 1);
    let destination = Ipv6Addr::new(0x2001, 0xDB8, 0, 0, 0, 0, 0, 2);
    // Hop-by-hop -> dest opts -> fragment -> ICMPv6.
    let mut chain = vec![
        ipv6::NEXT_HEADER_DEST_OPTS,
        0,
        1,
        4,
        0,
        0,
        0,
        0, // HBH with one PadN
        ipv6::NEXT_HEADER_FRAGMENT,
        0,
        1,
        4,
        0,
        0,
        0,
        0, // DstOpts with one PadN
        ipv6::NEXT_HEADER_ICMPV6,
        0,
        0,
        0x01,
        0,
        0,
        0,
        0x42, // Fragment: offset 0, more, id 0x42
    ];
    chain.extend_from_slice(&[0xEE; 8]);
    let mut packet = vec![0u8; ipv6::IPV6_HEADER_LEN + chain.len()];
    Ipv6Header::new(source, destination, ipv6::NEXT_HEADER_HOP_BY_HOP)
        .write(&mut packet, chain.len())
        .expect("seed header writes");
    packet[ipv6::IPV6_HEADER_LEN..].copy_from_slice(&chain);
    packet
}

#[test]
fn structured_inputs_with_corrupted_fields_never_panic() {
    let mut packet = valid_chain_packet();
    // The pristine packet parses and walks to its fragment header.
    let (header, payload) = Ipv6Header::parse(&packet).expect("seed packet parses");
    assert!(matches!(
        ipv6::walk(header.next_header, payload, false),
        Ok(ipv6::WalkOutcome::Fragment { .. })
    ));

    for byte in 0..packet.len() {
        for bit in 0..8u32 {
            packet[byte] ^= 1 << bit;
            if let Some((header, payload)) = Ipv6Header::parse(&packet) {
                exercise_walk(header.next_header, payload);
            }
            packet[byte] ^= 1 << bit;
        }
    }
}
