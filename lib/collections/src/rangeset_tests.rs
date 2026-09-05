//! [`RangeSet`] unit tests: the canonical form every operation must leave,
//! and the split/gap walk a caller drives.

use super::*;
use alloc::vec::Vec;

fn ranges(set: &RangeSet<u64>) -> Vec<(u64, u64)> {
    set.iter().map(|held| (held.start, held.end)).collect()
}

/// The invariant every operation must leave: ranges ascending, non-empty,
/// separated by at least one element, and the covered count exact.
fn assert_canonical(set: &RangeSet<u64>) {
    let mut covered = 0u64;
    let mut previous_end = None;
    for held in set {
        assert!(held.start < held.end, "an empty range at {}", held.start);
        if let Some(previous_end) = previous_end {
            assert!(
                held.start > previous_end,
                "ranges at {previous_end} and {} are adjacent or overlapping",
                held.start
            );
        }
        previous_end = Some(held.end);
        covered += held.end - held.start;
    }
    assert_eq!(set.covered(), covered, "the covered count drifted");
    assert_eq!(set.len(), set.iter().count(), "the range count drifted");
}

#[test]
fn an_insert_absorbs_every_range_it_touches() {
    let mut set: RangeSet<u64> = RangeSet::new();
    set.insert(10..15);
    set.insert(20..25);
    set.insert(30..35);
    assert_eq!(ranges(&set), [(10, 15), (20, 25), (30, 35)]);
    // Bridges the first two and touches the third's start.
    set.insert(15..30);
    assert_eq!(ranges(&set), [(10, 35)]);
    assert_canonical(&set);
}

#[test]
fn adjacent_ranges_coalesce_from_either_side() {
    let mut set: RangeSet<u64> = RangeSet::new();
    set.insert(100..101);
    set.insert(101..102);
    set.insert(99..100);
    assert_eq!(ranges(&set), [(99, 102)]);
    assert_eq!(set.len(), 1);
    assert_eq!(set.covered(), 3);
    assert_canonical(&set);
}

#[test]
fn a_repeated_or_contained_insert_changes_nothing() {
    let mut set: RangeSet<u64> = RangeSet::new();
    set.insert(8..12);
    set.insert(9..11);
    set.insert(8..12);
    assert_eq!(ranges(&set), [(8, 12)]);
    assert_canonical(&set);
}

#[test]
fn an_empty_insert_or_removal_is_a_no_op() {
    let mut set: RangeSet<u64> = RangeSet::new();
    set.insert(10..10);
    set.insert(Range { start: 20, end: 10 });
    assert!(set.is_empty() && set.covered() == 0);
    set.insert(10..20);
    set.remove(15..15);
    set.remove(Range { start: 20, end: 10 });
    assert_eq!(ranges(&set), [(10, 20)]);
    assert_canonical(&set);
}

#[test]
fn a_removal_splits_the_range_it_cuts() {
    let mut set: RangeSet<u64> = RangeSet::new();
    set.insert(0..100);
    set.remove(40..60);
    assert_eq!(ranges(&set), [(0, 40), (60, 100)]);
    assert_eq!(set.covered(), 80);
    // A cut that clears a whole range leaves nothing of it behind.
    set.remove(0..40);
    assert_eq!(ranges(&set), [(60, 100)]);
    // A cut spanning several ranges takes them all, trimming the ends.
    set.insert(0..20);
    set.insert(30..60);
    assert_eq!(ranges(&set), [(0, 20), (30, 100)]);
    set.remove(10..40);
    assert_eq!(ranges(&set), [(0, 10), (40, 100)]);
    assert_canonical(&set);
}

#[test]
fn a_removal_of_nothing_held_changes_nothing() {
    let mut set: RangeSet<u64> = RangeSet::new();
    set.insert(10..20);
    set.remove(0..10);
    set.remove(20..30);
    assert_eq!(ranges(&set), [(10, 20)]);
    assert_canonical(&set);
}

#[test]
fn contains_and_covering_answer_for_the_whole_range() {
    let mut set: RangeSet<u64> = RangeSet::new();
    set.insert(10..20);
    assert!(set.contains(10) && set.contains(19));
    assert!(!set.contains(9) && !set.contains(20));
    assert_eq!(set.covering(15), Some(10..20));
    assert_eq!(set.covering(20), None);
}

#[test]
fn first_overlap_and_first_gap_split_a_query_between_two_owners() {
    let mut set: RangeSet<u64> = RangeSet::new();
    set.insert(10..20);
    set.insert(30..40);
    // Held parts are clamped to the query, gaps are what is left.
    assert_eq!(set.first_overlap(0..50), Some(10..20));
    assert_eq!(set.first_overlap(15..17), Some(15..17));
    assert_eq!(set.first_overlap(25..35), Some(30..35));
    assert_eq!(set.first_overlap(20..30), None);
    assert_eq!(set.first_gap(0..50), Some(0..10));
    assert_eq!(set.first_gap(10..50), Some(20..30));
    assert_eq!(set.first_gap(12..18), None, "wholly held");
    assert_eq!(set.first_gap(35..40), None);
    assert_eq!(set.first_gap(35..45), Some(40..45));
    assert_eq!(set.first_gap(5..5), None);

    // Alternating the two walks a query without visiting an element, and the
    // parts reconstruct it exactly.
    let (mut cursor, end) = (0u64, 50u64);
    let mut parts: Vec<(u64, u64, bool)> = Vec::new();
    while cursor < end {
        let (part, held) = match set.first_gap(cursor..end) {
            Some(gap) if gap.start == cursor => (gap, false),
            _ => (set.first_overlap(cursor..end).expect("held here"), true),
        };
        parts.push((part.start, part.end, held));
        cursor = part.end;
    }
    assert_eq!(
        parts,
        [
            (0, 10, false),
            (10, 20, true),
            (20, 30, false),
            (30, 40, true),
            (40, 50, false)
        ]
    );
}

#[test]
fn pop_first_takes_the_lowest_range_and_discharges_it() {
    let mut set: RangeSet<u64> = RangeSet::new();
    set.insert(30..35);
    set.insert(10..12);
    assert_eq!(set.covered(), 7);
    assert_eq!(set.pop_first(), Some(10..12));
    assert_eq!(set.covered(), 5);
    assert_eq!(set.pop_first(), Some(30..35));
    assert_eq!(set.pop_first(), None);
    assert_canonical(&set);
}

#[test]
fn clear_drops_everything_and_the_count_with_it() {
    let mut set: RangeSet<u64> = RangeSet::new();
    set.insert(0..1000);
    assert_eq!(set.covered(), 1000);
    set.clear();
    assert!(set.is_empty() && set.covered() == 0 && set.iter().next().is_none());
    assert_canonical(&set);
}

#[test]
fn iteration_is_ascending_from_both_ends() {
    let mut set: RangeSet<u64> = RangeSet::new();
    set.insert(30..35);
    set.insert(10..15);
    set.insert(20..25);
    let forward: Vec<u64> = set.iter().map(|held| held.start).collect();
    assert_eq!(forward, [10, 20, 30]);
    let backward: Vec<u64> = set.iter().rev().map(|held| held.start).collect();
    assert_eq!(backward, [30, 20, 10]);
    assert_eq!(set.iter().len(), 3);
    assert!((&set).into_iter().map(|held| held.start).eq(forward));
}

#[test]
fn a_run_costs_one_entry_however_many_elements_it_holds() {
    // The property the whole container exists for: a hundred-terabyte extent
    // released as one run costs one entry, not one per block.
    // Miri interprets every operation, so it appends a handful of blocks
    // rather than a thousand; the property is the entry count, not the run.
    let appended: u64 = if cfg!(miri) { 8 } else { 1_000 };
    let mut set: RangeSet<u64> = RangeSet::new();
    set.insert(0..25_000_000_000);
    assert_eq!(set.len(), 1);
    assert_eq!(set.covered(), 25_000_000_000);
    // Appending each further block *contiguously* still costs one entry,
    // because every insert coalesces into the one held run.
    for block in 25_000_000_000..25_000_000_000 + appended {
        set.insert(block..block + 1);
    }
    assert_eq!(set.len(), 1, "a contiguous stream stays one entry");
    assert_eq!(set.covered(), 25_000_000_000 + appended);
    assert_canonical(&set);
}

#[test]
fn a_slot_keyed_set_coalesces_the_same_way() {
    let mut set: RangeSet<usize> = RangeSet::new();
    set.insert(4..8);
    set.insert(0..4);
    assert_eq!(set.len(), 1);
    assert_eq!(set.covering(7), Some(0..8));
    assert_eq!(set.covered(), 8);
}

#[test]
fn the_top_of_the_key_range_neither_wraps_nor_panics() {
    let mut set: RangeSet<u64> = RangeSet::new();
    set.insert(u64::MAX - 4..u64::MAX);
    assert_eq!(set.covered(), 4);
    assert!(set.contains(u64::MAX - 1));
    set.insert(u64::MAX - 8..u64::MAX - 4);
    assert_eq!(ranges(&set), [(u64::MAX - 8, u64::MAX)]);
    assert_eq!(set.first_gap(0..u64::MAX), Some(0..u64::MAX - 8));
    set.remove(u64::MAX - 6..u64::MAX - 2);
    assert_eq!(
        ranges(&set),
        [(u64::MAX - 8, u64::MAX - 6), (u64::MAX - 2, u64::MAX)]
    );
    assert_canonical(&set);
}

#[test]
fn every_operation_matches_a_per_element_model() {
    // A range set is only worth having if it answers exactly what a set of
    // elements would, so it is checked against one: a deterministic sequence
    // of inserts and removals over a small window, with the canonical form,
    // the covered count, membership, and both walk primitives compared to a
    // flat element set after every step.
    // The per-step check probes every query in the window, so the cost is
    // cubic in it. Miri interprets every operation and is hunting undefined
    // behaviour, of which this tier has no `unsafe` source, so it drives a
    // narrow window over few steps; the wide search is the ordinary stage's.
    const WINDOW: u64 = if cfg!(miri) { 8 } else { 48 };
    const STEPS: u32 = if cfg!(miri) { 24 } else { 600 };
    let mut set: RangeSet<u64> = RangeSet::new();
    let mut model = alloc::collections::BTreeSet::new();
    // xorshift, so the sequence is fixed and reproducible without a clock.
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for step in 0..STEPS {
        let raw = next();
        let start = raw % WINDOW;
        let len = 1 + (raw >> 8) % 9;
        let end = (start + len).min(WINDOW);
        let inserting = (raw >> 16) & 1 == 0;
        if inserting {
            set.insert(start..end);
        } else {
            set.remove(start..end);
        }
        for element in start..end {
            if inserting {
                model.insert(element);
            } else {
                model.remove(&element);
            }
        }

        assert_canonical(&set);
        let held = u64::try_from(model.len()).expect("the window is small");
        assert_eq!(set.covered(), held, "step {step}");
        for element in 0..WINDOW {
            assert_eq!(
                set.contains(element),
                model.contains(&element),
                "step {step}, element {element}"
            );
        }
        // The two walk primitives must agree with the model about the first
        // held and the first unheld part of every query.
        for probe in 0..WINDOW {
            let run_from = |from: u64, held: bool| {
                let to = (from..WINDOW)
                    .find(|e| model.contains(e) != held)
                    .unwrap_or(WINDOW);
                from..to
            };
            assert_eq!(
                set.first_overlap(probe..WINDOW),
                (probe..WINDOW)
                    .find(|e| model.contains(e))
                    .map(|from| run_from(from, true)),
                "step {step}, overlap at {probe}"
            );
            assert_eq!(
                set.first_gap(probe..WINDOW),
                (probe..WINDOW)
                    .find(|e| !model.contains(e))
                    .map(|from| run_from(from, false)),
                "step {step}, gap at {probe}"
            );
        }
    }
}

#[test]
fn one_element_of_separation_is_a_gap_and_closing_it_merges() {
    let mut set: RangeSet<u64> = RangeSet::new();
    set.insert(1 << 20..1 << 21);
    assert_eq!(set.len(), 1);
    set.insert((1 << 21) + 1..(1 << 21) + 2);
    assert_eq!(set.len(), 2, "one element of separation is a gap");
    set.insert(1 << 21..(1 << 21) + 1);
    assert_eq!(set.len(), 1, "closing the gap merges them");
    assert_eq!(set.covered(), (1 << 20) + 2);
    assert_canonical(&set);
}
