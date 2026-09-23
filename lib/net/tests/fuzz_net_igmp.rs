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

use tairix_fuzzseed::Prng;
use tairix_net::igmp::{IgmpMessage, IGMP_MESSAGE_LEN};
use tairix_net::Ipv4Addr;

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

fn exercise_round_trip(rng: &mut Prng) {
    let group = Ipv4Addr::from(rng.next_u32().to_be_bytes());
    let message = match rng.next_u64() % 4 {
        0 => IgmpMessage::MembershipQuery {
            max_resp_deciseconds: rng.next_u8(),
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

#[test]
fn random_inputs_never_panic() {
    let mut rng = Prng::new(tairix_fuzzseed::start(
        "random_inputs_never_panic",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    ));
    let mut buf = [0u8; 24];
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);
    loop {
        for _ in 0..SMOKE_ITERATIONS {
            let size = rng.at_most(buf.len());
            rng.fill(&mut buf[..size]);
            exercise_parse(&buf[..size]);
            exercise_round_trip(&mut rng);
        }
        if !tairix_fuzzseed::within_budget(deadline) {
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
