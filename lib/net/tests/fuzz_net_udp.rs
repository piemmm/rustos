//! Deterministic fuzz harness for the dual-stack UDP codec.
//!
//! Invariants, for any input bits a peer crafts:
//!
//! 1. Parsing never panics, for either family, on any bytes.
//! 2. A datagram produced by [`udp::write`] parses back to the same ports
//!    and payload under the same pseudo-header (round-trip).
//! 3. A parsed datagram's payload always lies within the input bytes and
//!    respects the eight-byte-header lower bound.
//!
//! Runs the fixed smoke sweep under plain `cargo test`; keeps drawing from
//! the same seeded stream until `TAIRIX_FUZZ_BUDGET_SECS` elapses under
//! `cargo xtask fuzz`.

use tairix_net::udp::{self, Pseudo, UdpDatagram, UDP_HEADER_LEN};
use tairix_net::{Ipv4Addr, Ipv6Addr};

/// Fixed-iteration sweep run once by a plain `cargo test` (no budget set).
const SMOKE_ITERATIONS: u64 = 20_000;

fn pseudo(rng: &mut Lcg) -> Pseudo {
    if rng.next_u64().is_multiple_of(2) {
        Pseudo::V4 {
            source: Ipv4Addr::from(((rng.next_u64() & 0xFFFF_FFFF) as u32).to_be_bytes()),
            destination: Ipv4Addr::from(((rng.next_u64() & 0xFFFF_FFFF) as u32).to_be_bytes()),
        }
    } else {
        let mut octets = [0u8; 16];
        rng.fill(&mut octets);
        let source = Ipv6Addr::from(octets);
        rng.fill(&mut octets);
        Pseudo::V6 {
            source,
            destination: Ipv6Addr::from(octets),
        }
    }
}

fn exercise_parse(p: Pseudo, bytes: &[u8]) {
    if let Some(dg) = UdpDatagram::parse(p, bytes) {
        // The payload is always inside the input and past the header.
        assert!(bytes.len() >= UDP_HEADER_LEN);
        assert!(dg.payload.len() <= bytes.len() - UDP_HEADER_LEN);
    }
}

fn exercise_round_trip(rng: &mut Lcg, p: Pseudo) {
    let src_port = (rng.next_u64() & 0xFFFF) as u16;
    let dst_port = (rng.next_u64() & 0xFFFF) as u16;
    let len = (rng.next_u64() & 0x1FF) as usize;
    let mut payload = vec![0u8; len];
    rng.fill(&mut payload);
    let mut out = vec![0u8; UDP_HEADER_LEN + len];
    udp::write(p, src_port, dst_port, &payload, &mut out).expect("write within bounds");
    let dg = UdpDatagram::parse(p, &out).expect("a freshly written datagram parses");
    assert_eq!(dg.source_port, src_port);
    assert_eq!(dg.destination_port, dst_port);
    assert_eq!(dg.payload, &payload[..]);
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
    let mut rng = Lcg::new(tairix_fuzzseed::start(
        "random_inputs_never_panic",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    ));
    let mut buf = [0u8; 128];
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);
    loop {
        for _ in 0..SMOKE_ITERATIONS {
            let p = pseudo(&mut rng);
            let size = ((rng.next_u64() & 0x7F) as usize) % (buf.len() + 1);
            rng.fill(&mut buf[..size]);
            exercise_parse(p, &buf[..size]);
            exercise_round_trip(&mut rng, p);
        }
        if !tairix_fuzzseed::within_budget(deadline) {
            break;
        }
    }
}

#[test]
fn corrupted_fields_never_panic() {
    // Bit-flip every bit of a valid datagram to walk the accept/reject
    // boundary of the length and checksum checks.
    let p = Pseudo::V4 {
        source: Ipv4Addr::new(10, 0, 2, 2),
        destination: Ipv4Addr::new(10, 0, 2, 15),
    };
    let mut datagram = vec![0u8; UDP_HEADER_LEN + 16];
    udp::write(p, 4444, 53, &[0xA5; 16], &mut datagram).expect("seed datagram writes");
    UdpDatagram::parse(p, &datagram).expect("seed datagram parses");
    for byte in 0..datagram.len() {
        for bit in 0..8u32 {
            datagram[byte] ^= 1 << bit;
            exercise_parse(p, &datagram);
            datagram[byte] ^= 1 << bit;
        }
    }
}
