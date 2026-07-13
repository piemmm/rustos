//! Unit and property tests for the fragment reassembler.

use super::*;
use crate::addr::{Ipv4Addr, Ipv6Addr};
use alloc::vec;
use alloc::vec::Vec;

fn key_v4(id: u32) -> FragKey {
    FragKey {
        source: IpAddr::V4(Ipv4Addr::new(10, 0, 2, 2)),
        destination: IpAddr::V4(Ipv4Addr::new(10, 0, 2, 15)),
        identification: id,
        protocol: 17,
    }
}

fn key_v6(id: u32) -> FragKey {
    FragKey {
        source: IpAddr::V6(Ipv6Addr::new(0xFE80, 0, 0, 0, 0, 0, 0, 1)),
        destination: IpAddr::V6(Ipv6Addr::new(0xFE80, 0, 0, 0, 0, 0, 0, 2)),
        identification: id,
        protocol: 0,
    }
}

fn source_v4(last: u8) -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(10, 0, 2, last))
}

fn now() -> Duration64 {
    Duration64::from_secs(100)
}

#[test]
fn two_fragments_reassemble_in_order_and_out_of_order() {
    let payload: Vec<u8> = (0u8..24).collect();
    for keys in [key_v4(1), key_v6(1)] {
        for order in [[0usize, 1], [1, 0]] {
            let mut reassembler = Reassembler::new(ReassemblyConfig::default());
            let pieces = [
                (0usize, true, &payload[..16]),
                (16usize, false, &payload[16..]),
            ];
            let mut complete = None;
            for &i in &order {
                let (offset, more, data) = pieces[i];
                match reassembler.push(keys, offset, more, data, now()) {
                    PushOutcome::Complete(bytes) => complete = Some(bytes),
                    PushOutcome::Pending => {}
                    rejected @ PushOutcome::Rejected(_) => {
                        panic!("unexpected outcome: {rejected:?}")
                    }
                }
            }
            assert_eq!(complete.expect("completes"), payload);
            assert!(reassembler.is_empty());
        }
    }
}

#[test]
fn unfragmented_single_piece_completes_immediately() {
    let mut reassembler = Reassembler::new(ReassemblyConfig::default());
    let payload = [7u8; 21];
    match reassembler.push(key_v4(9), 0, false, &payload, now()) {
        PushOutcome::Complete(bytes) => assert_eq!(bytes, payload),
        other => panic!("unexpected outcome: {other:?}"),
    }
}

#[test]
fn overlap_drops_the_whole_datagram() {
    let mut reassembler = Reassembler::new(ReassemblyConfig::default());
    let key = key_v4(2);
    assert_eq!(
        reassembler.push(key, 0, true, &[1; 16], now()),
        PushOutcome::Pending
    );
    // Partial overlap.
    assert_eq!(
        reassembler.push(key, 8, true, &[2; 16], now()),
        PushOutcome::Rejected(RejectReason::Overlap)
    );
    assert!(reassembler.is_empty());
    // Exact duplicate is an overlap too.
    assert_eq!(
        reassembler.push(key, 0, true, &[1; 16], now()),
        PushOutcome::Pending
    );
    assert_eq!(
        reassembler.push(key, 0, true, &[1; 16], now()),
        PushOutcome::Rejected(RejectReason::Overlap)
    );
    assert!(reassembler.is_empty());
}

#[test]
fn malformed_pieces_are_rejected() {
    let mut reassembler = Reassembler::new(ReassemblyConfig::default());
    // Empty fragment.
    assert_eq!(
        reassembler.push(key_v4(3), 0, true, &[], now()),
        PushOutcome::Rejected(RejectReason::Malformed)
    );
    // Non-final fragment not a multiple of 8.
    assert_eq!(
        reassembler.push(key_v4(3), 0, true, &[0; 12], now()),
        PushOutcome::Rejected(RejectReason::Malformed)
    );
    // Beyond the 16-bit datagram bound.
    assert_eq!(
        reassembler.push(key_v4(3), 65_528, false, &[0; 16], now()),
        PushOutcome::Rejected(RejectReason::Malformed)
    );
    assert!(reassembler.is_empty());
}

#[test]
fn contradictory_final_lengths_drop_the_datagram() {
    let mut reassembler = Reassembler::new(ReassemblyConfig::default());
    let key = key_v6(4);
    assert_eq!(
        reassembler.push(key, 32, false, &[1; 8], now()),
        PushOutcome::Pending
    );
    // A different final length contradicts the first.
    assert_eq!(
        reassembler.push(key, 48, false, &[2; 8], now()),
        PushOutcome::Rejected(RejectReason::Malformed)
    );
    assert!(reassembler.is_empty());
    // A piece beyond a known final length is equally contradictory.
    assert_eq!(
        reassembler.push(key, 32, false, &[1; 8], now()),
        PushOutcome::Pending
    );
    assert_eq!(
        reassembler.push(key, 40, true, &[2; 8], now()),
        PushOutcome::Rejected(RejectReason::Malformed)
    );
    assert!(reassembler.is_empty());
}

#[test]
fn fragment_count_is_bounded() {
    let config = ReassemblyConfig {
        max_fragments: 4,
        ..ReassemblyConfig::default()
    };
    let mut reassembler = Reassembler::new(config);
    let key = key_v4(5);
    for i in 0..4u32 {
        assert_eq!(
            reassembler.push(key, (i as usize) * 16, true, &[0; 8], now()),
            PushOutcome::Pending
        );
    }
    assert_eq!(
        reassembler.push(key, 64, true, &[0; 8], now()),
        PushOutcome::Rejected(RejectReason::TooManyFragments)
    );
    assert!(reassembler.is_empty());
}

#[test]
fn expiry_reports_first_fragment_fact() {
    let mut reassembler = Reassembler::new(ReassemblyConfig::default());
    let with_first = key_v4(6);
    let without_first = key_v4(7);
    assert_eq!(
        reassembler.push(with_first, 0, true, &[0; 8], now()),
        PushOutcome::Pending
    );
    assert_eq!(
        reassembler.push(without_first, 8, true, &[0; 8], now()),
        PushOutcome::Pending
    );
    assert!(reassembler.next_deadline().is_some());
    // Nothing expires before the deadline.
    assert!(reassembler.advance(now()).is_empty());
    let late = Duration64::from_secs(100 + 61);
    let mut expired = reassembler.advance(late);
    expired.sort_by_key(|e| e.key.identification);
    assert_eq!(
        expired,
        vec![
            ExpiredDatagram {
                key: with_first,
                first_fragment_received: true,
            },
            ExpiredDatagram {
                key: without_first,
                first_fragment_received: false,
            },
        ]
    );
    assert!(reassembler.is_empty());
    assert!(reassembler.next_deadline().is_none());
}

#[test]
fn datagram_count_evicts_oldest_first() {
    let config = ReassemblyConfig {
        max_datagrams: 2,
        ..ReassemblyConfig::default()
    };
    let mut reassembler = Reassembler::new(config);
    let oldest = key_v4(1);
    let newer = key_v4(2);
    assert_eq!(
        reassembler.push(oldest, 0, true, &[0; 8], Duration64::from_secs(1)),
        PushOutcome::Pending
    );
    assert_eq!(
        reassembler.push(newer, 0, true, &[0; 8], Duration64::from_secs(2)),
        PushOutcome::Pending
    );
    // A third datagram evicts the oldest.
    assert_eq!(
        reassembler.push(key_v4(3), 0, true, &[0; 8], Duration64::from_secs(3)),
        PushOutcome::Pending
    );
    assert_eq!(reassembler.len(), 2);
    // The oldest is gone: completing it now needs both pieces again.
    assert_eq!(
        reassembler.push(oldest, 8, false, &[0; 8], Duration64::from_secs(4)),
        PushOutcome::Pending
    );
}

#[test]
fn per_source_budget_evicts_that_source_first() {
    let config = ReassemblyConfig {
        per_source_budget: 64,
        global_budget: 1024,
        ..ReassemblyConfig::default()
    };
    let mut reassembler = Reassembler::new(config);
    let attacker_a = FragKey {
        source: source_v4(66),
        ..key_v4(1)
    };
    let attacker_b = FragKey {
        source: source_v4(66),
        ..key_v4(2)
    };
    let victim = FragKey {
        source: source_v4(77),
        ..key_v4(3)
    };
    assert_eq!(
        reassembler.push(attacker_a, 0, true, &[0; 64], Duration64::from_secs(1)),
        PushOutcome::Pending
    );
    assert_eq!(
        reassembler.push(victim, 0, true, &[0; 32], Duration64::from_secs(2)),
        PushOutcome::Pending
    );
    // The same source's second datagram evicts its first, not the
    // other source's.
    assert_eq!(
        reassembler.push(attacker_b, 0, true, &[0; 64], Duration64::from_secs(3)),
        PushOutcome::Pending
    );
    assert_eq!(reassembler.len(), 2);
    // The victim's state survived: its final piece completes it.
    let mut expected = vec![0u8; 32];
    expected.extend_from_slice(&[1; 16]);
    assert_eq!(
        reassembler.push(victim, 32, false, &[1; 16], Duration64::from_secs(4)),
        PushOutcome::Complete(expected)
    );
}

#[test]
fn oversize_single_datagram_is_refused_within_budget() {
    let config = ReassemblyConfig {
        per_source_budget: 16,
        global_budget: 16,
        ..ReassemblyConfig::default()
    };
    let mut reassembler = Reassembler::new(config);
    assert_eq!(
        reassembler.push(key_v4(1), 0, true, &[0; 32], now()),
        PushOutcome::Rejected(RejectReason::BudgetExceeded)
    );
}

#[test]
fn zero_datagram_cap_fails_closed() {
    let config = ReassemblyConfig {
        max_datagrams: 0,
        ..ReassemblyConfig::default()
    };
    let mut reassembler = Reassembler::new(config);
    assert_eq!(
        reassembler.push(key_v4(1), 0, true, &[0; 8], now()),
        PushOutcome::Rejected(RejectReason::BudgetExceeded)
    );
}

/// Deterministic LCG for the property sweeps (the fuzz-harness
/// generator, reused so failures reproduce the same way).
struct Lcg(u64);

impl Lcg {
    fn next_u64(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        self.0
    }
}

#[test]
fn property_random_splits_reassemble_exactly() {
    let mut rng = Lcg(0xD15_EA5E);
    for round in 0..200u32 {
        let len = 1 + ((rng.next_u64() & 0xFFF) as usize);
        let payload: Vec<u8> = (0..len)
            .map(|i| i.to_le_bytes()[0] ^ round.to_le_bytes()[0])
            .collect();
        // Split into 8-byte-aligned pieces.
        let mut cuts = vec![0usize, len];
        for _ in 0..(rng.next_u64() % 8) {
            cuts.push((((rng.next_u64() & 0xFFFF) as usize) % len) & !7);
        }
        cuts.sort_unstable();
        cuts.dedup();
        let mut pieces: Vec<(usize, usize)> = cuts.windows(2).map(|w| (w[0], w[1])).collect();
        // Shuffle the pieces deterministically.
        for i in (1..pieces.len()).rev() {
            let j = ((rng.next_u64() & 0xFFFF) as usize) % (i + 1);
            pieces.swap(i, j);
        }
        let key = key_v4(round);
        let mut reassembler = Reassembler::new(ReassemblyConfig::default());
        let mut complete = None;
        for &(start, end) in &pieces {
            let more = end != len;
            match reassembler.push(key, start, more, &payload[start..end], now()) {
                PushOutcome::Complete(bytes) => complete = Some(bytes),
                PushOutcome::Pending => {}
                rejected @ PushOutcome::Rejected(_) => {
                    panic!("unexpected outcome: {rejected:?}")
                }
            }
        }
        assert_eq!(complete.expect("completes"), payload);
    }
}

#[test]
fn property_budgets_never_exceeded_under_random_load() {
    let config = ReassemblyConfig {
        per_source_budget: 4 * 1024,
        global_budget: 16 * 1024,
        max_datagrams: 16,
        ..ReassemblyConfig::default()
    };
    let mut reassembler = Reassembler::new(config);
    let mut rng = Lcg(0xFEED_FACE);
    for step in 0..5_000i64 {
        let id = (rng.next_u64() & 0x1F) as u32;
        let source = source_v4((rng.next_u64() & 0x3) as u8);
        let key = FragKey {
            source,
            ..key_v4(id)
        };
        let offset = ((rng.next_u64() & 0x1FFF) as usize) & !7;
        let len = 8 + (((rng.next_u64() & 0x1FF) as usize) & !7);
        let more = rng.next_u64() % 4 != 0;
        let data = vec![step.to_le_bytes()[0]; len];
        let _ = reassembler.push(key, offset, more, &data, Duration64::from_secs(1 + step));
        // The invariants under test: every budget holds after every
        // push, whatever the outcome was.
        assert!(reassembler.buffered_bytes() <= config.global_budget);
        assert!(reassembler.len() <= config.max_datagrams);
    }
}
