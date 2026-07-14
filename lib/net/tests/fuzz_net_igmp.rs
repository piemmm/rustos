//! Deterministic fuzz harness for the IGMPv2 codec.
//!
//! Invariants, for any input bits a peer crafts:
//!
//! 1. [`IgmpMessage::parse`] never panics, on any bytes.
//! 2. A message produced by [`IgmpMessage::write`] parses back to the
//!    same message (round-trip).
//! 3. A parsed message always re-encodes to eight bytes that re-parse
//!    identically (idempotent, so the checksum it accepted it also
//!    reproduces).
//!
//! Runs a fixed smoke sweep under plain `cargo test`; keeps drawing from
//! the same seeded stream until the budget elapses under
//! `cargo xtask fuzz`.

use rustos_net::igmp::{IgmpMessage, IGMP_MESSAGE_LEN};
use rustos_net::Ipv4Addr;

/// Fixed-iteration sweep run once by a plain `cargo test` (no budget set).
const SMOKE_ITERATIONS: u64 = 20_000;

fn exercise_parse(bytes: &[u8]) {
    if let Some(message) = IgmpMessage::parse(bytes) {
        // A parsed message re-encodes and re-parses to itself.
        let mut out = [0u8; IGMP_MESSAGE_LEN];
        IgmpMessage::write(&message, &mut out).expect("write fits the fixed buffer");
        assert_eq!(IgmpMessage::parse(&out), Some(message));
    }
}

fn exercise_round_trip(rng: &mut Lcg) {
    let group = Ipv4Addr::from(((rng.next_u64() & 0xFFFF_FFFF) as u32).to_be_bytes());
    let message = match rng.next_u64() % 4 {
        0 => IgmpMessage::MembershipQuery {
            max_resp_deciseconds: (rng.next_u64() & 0xFF) as u8,
            group,
        },
        1 => IgmpMessage::V2Report { group },
        2 => IgmpMessage::V1Report { group },
        _ => IgmpMessage::LeaveGroup { group },
    };
    let mut out = [0u8; IGMP_MESSAGE_LEN];
    IgmpMessage::write(&message, &mut out).expect("write");
    assert_eq!(IgmpMessage::parse(&out), Some(message));
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
    let mut buf = [0u8; 24];
    let deadline = rustos_fuzzseed::budget_deadline(rustos_fuzzseed::FUZZ_BUDGET_ENV);
    loop {
        for _ in 0..SMOKE_ITERATIONS {
            let size = ((rng.next_u64() & 0x1F) as usize) % (buf.len() + 1);
            rng.fill(&mut buf[..size]);
            exercise_parse(&buf[..size]);
            exercise_round_trip(&mut rng);
        }
        if !rustos_fuzzseed::within_budget(deadline) {
            break;
        }
    }
}

#[test]
fn corrupted_fields_never_panic() {
    // Bit-flip every bit of a valid report to walk the checksum and type
    // accept/reject boundary.
    let message = IgmpMessage::V2Report {
        group: Ipv4Addr::new(239, 1, 2, 3),
    };
    let mut bytes = [0u8; IGMP_MESSAGE_LEN];
    IgmpMessage::write(&message, &mut bytes).expect("seed writes");
    IgmpMessage::parse(&bytes).expect("seed parses");
    for byte in 0..bytes.len() {
        for bit in 0..8u32 {
            bytes[byte] ^= 1 << bit;
            exercise_parse(&bytes);
            bytes[byte] ^= 1 << bit;
        }
    }
}
