//! Tests for the control-group scan.

use super::{
    is_full, resolve, scan, scan_portable, Group, GroupMatch, BASELINE_NAME, CANDIDATES, DELETED,
    EMPTY, GROUP_LEN, TAG_MASK, VECTORS,
};
use tairix_abi::cpufeatures::CpuFeatureSet;

/// The obvious lane-by-lane answer, written independently of the word tricks
/// and the vector intrinsics so it can arbitrate between them.
fn scan_naive(group: &Group, tag: u8) -> GroupMatch {
    let mut out = GroupMatch::default();
    for (lane, &ctrl) in group.iter().enumerate() {
        let bit = 1u16 << lane;
        if ctrl == tag {
            out.tag |= bit;
        }
        if ctrl == EMPTY {
            out.empty |= bit;
        }
        if !is_full(ctrl) {
            out.free |= bit;
        }
    }
    out
}

#[test]
fn control_values_are_distinguishable() {
    assert!(!is_full(EMPTY));
    assert!(!is_full(DELETED));
    assert!(is_full(0));
    assert!(is_full(TAG_MASK));
    assert_ne!(
        EMPTY, DELETED,
        "the probe terminator must not be a tombstone"
    );
}

/// A borrow crossing a lane boundary is the classic way a word-at-a-time
/// equality test forges a match, and it would make the baseline disagree with
/// the exact vector candidates. Sweep every adjacent-lane byte pair against
/// the naive answer.
#[test]
fn portable_scan_is_exact_across_lane_boundaries() {
    // Miri interprets every operation and is looking for undefined behaviour,
    // which this all-safe sweep cannot contain; it strides the byte space
    // there and sweeps it exhaustively in the ordinary test run.
    let stride = if cfg!(miri) { 17 } else { 1 };
    for first in (0u16..=255).step_by(stride) {
        for second in (0u16..=255).step_by(stride) {
            let mut group = [0u8; GROUP_LEN];
            #[allow(clippy::cast_possible_truncation)] // both loops range over a byte
            {
                group[7] = first as u8;
                group[8] = second as u8;
            }
            for tag in [0u8, 1, 0x2a, TAG_MASK] {
                assert_eq!(
                    scan_portable(&group, tag),
                    scan_naive(&group, tag),
                    "group {group:02x?} tag {tag:#04x}",
                );
            }
        }
    }
}

/// Every lane position, for every control class, in both halves of the word
/// pair the baseline splits the group into.
#[test]
fn portable_scan_agrees_with_the_naive_answer_at_every_lane() {
    for lane in 0..GROUP_LEN {
        for filler in [EMPTY, DELETED, 0x00, 0x2a, TAG_MASK] {
            for planted in [EMPTY, DELETED, 0x00, 0x2a, TAG_MASK] {
                let mut group = [filler; GROUP_LEN];
                group[lane] = planted;
                for tag in [0u8, 0x2a, TAG_MASK] {
                    assert_eq!(
                        scan_portable(&group, tag),
                        scan_naive(&group, tag),
                        "lane {lane} filler {filler:#04x} planted {planted:#04x} tag {tag:#04x}",
                    );
                }
            }
        }
    }
}

/// The self-verify vectors are the gate every candidate passes, so they must
/// themselves be right: check the reference they pin against the naive answer.
#[test]
fn self_verify_vectors_pin_the_naive_answer() {
    for vector in VECTORS {
        assert_eq!(
            scan_portable(&vector.group, vector.tag),
            scan_naive(&vector.group, vector.tag),
        );
    }
}

/// Every vector candidate compiled on this build must reproduce the portable
/// reference on every self-verify vector — the same check the selector
/// enforces, asserted directly so a broken candidate fails the crate's own
/// tests and not only its selection.
#[test]
fn candidates_match_the_reference_on_every_vector() {
    for candidate in CANDIDATES {
        for vector in VECTORS {
            assert_eq!(
                (candidate.impl_)(&vector.group, vector.tag),
                scan_naive(&vector.group, vector.tag),
                "candidate {} disagrees with the reference",
                candidate.name,
            );
        }
    }
}

/// The self-verify vectors are a fixed, small set; sweep the candidates over
/// the same exhaustive lane grid the baseline is held to, so a candidate that
/// is right on the vectors but wrong on some lane is still caught.
#[test]
fn candidates_match_the_reference_at_every_lane() {
    for candidate in CANDIDATES {
        for lane in 0..GROUP_LEN {
            for filler in [EMPTY, DELETED, 0x00, 0x2a, TAG_MASK] {
                for planted in [EMPTY, DELETED, 0x00, 0x2a, TAG_MASK] {
                    let mut group = [filler; GROUP_LEN];
                    group[lane] = planted;
                    for tag in [0u8, 0x2a, TAG_MASK] {
                        assert_eq!(
                            (candidate.impl_)(&group, tag),
                            scan_naive(&group, tag),
                            "candidate {} lane {lane}",
                            candidate.name,
                        );
                    }
                }
            }
        }
    }
}

/// Selection is process-global, so one test owns the whole sequence: the
/// pre-resolve scan is the baseline, an empty feature set resolves to the
/// baseline, and the resolved scan still answers correctly.
#[test]
fn resolution_falls_closed_to_the_baseline() {
    let group = [EMPTY; GROUP_LEN];
    assert_eq!(scan(&group, 0), scan_naive(&group, 0));

    let decision = resolve(CpuFeatureSet::EMPTY);
    assert_eq!(decision.chosen, BASELINE_NAME);
    assert_eq!(scan(&group, 0), scan_naive(&group, 0));
}
