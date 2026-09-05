//! [`RangeMap`] unit tests: the disjointness invariant, the neighbour probes,
//! and the first-fit gap search.

use super::*;

/// Every range the map holds, so a test can assert the whole shape.
fn ranges<V>(map: &RangeMap<u64, V>) -> alloc::vec::Vec<(u64, u64)> {
    map.iter().map(|(held, _)| (held.start, held.end)).collect()
}

#[test]
fn a_span_refuses_an_empty_or_overflowing_count() {
    assert_eq!(10u64.span(4), Some(10..14));
    assert_eq!(10u64.span(0), None);
    assert_eq!(u64::MAX.span(1), None);
    assert_eq!((u64::MAX - 1).span(1), Some(u64::MAX - 1..u64::MAX));
    assert_eq!(10usize.span(4), Some(10..14));
    assert_eq!(usize::MAX.span(1), None);
    assert_eq!((usize::MAX - 1).span(2), None);
}

#[test]
fn a_distance_saturates_rather_than_wrapping() {
    assert_eq!(14u64.distance_from(10), 4);
    assert_eq!(10u64.distance_from(14), 0);
    assert_eq!(u64::MAX.distance_from(0), u64::MAX);
    assert_eq!(14usize.distance_from(10), 4);
    assert_eq!(10usize.distance_from(14), 0);
}

#[test]
fn an_insert_refuses_an_empty_range_and_every_overlap() {
    let mut map: RangeMap<u64, char> = RangeMap::new();
    assert_eq!(map.insert(10..10, 'a'), Err(RangeError::Empty));
    let inverted = Range { start: 20, end: 10 };
    assert_eq!(map.insert(inverted, 'a'), Err(RangeError::Empty));
    map.insert(10..20, 'a').expect("the first range");
    // Every shape of overlap: identical, contained, containing, straddling
    // either end.
    for overlap in [10..20, 12..15, 5..25, 5..15, 15..25, 19..21] {
        assert_eq!(map.insert(overlap, 'b'), Err(RangeError::Overlap));
    }
    // Abutting on either side is not an overlap: an entry is an identity, so
    // neighbours stay distinct.
    map.insert(20..30, 'b').expect("abuts above");
    map.insert(5..10, 'c').expect("abuts below");
    assert_eq!(ranges(&map), [(5, 10), (10, 20), (20, 30)]);
    assert_eq!(map.len(), 3);
}

#[test]
fn covering_names_the_one_range_holding_a_point() {
    let mut map: RangeMap<u64, char> = RangeMap::new();
    map.insert(10..20, 'a').expect("fits");
    map.insert(30..40, 'b').expect("fits");
    assert_eq!(map.covering(10).map(|(_, v)| *v), Some('a'));
    assert_eq!(map.covering(19).map(|(_, v)| *v), Some('a'));
    assert!(map.covering(20).is_none(), "the exclusive end is outside");
    assert!(map.covering(29).is_none(), "the gap is outside");
    assert_eq!(map.covering(30).map(|(_, v)| *v), Some('b'));
    assert!(map.covering(9).is_none(), "below every range");
    assert!(map.covering(40).is_none(), "above every range");
    assert_eq!(map.covering(15).map(|(held, _)| held), Some(10..20));
}

#[test]
fn get_and_remove_name_a_range_by_its_start_alone() {
    let mut map: RangeMap<u64, char> = RangeMap::new();
    map.insert(10..20, 'a').expect("fits");
    assert_eq!(map.get(10).map(|(held, v)| (held, *v)), Some((10..20, 'a')));
    assert!(map.get(11).is_none(), "an interior address is not a start");
    assert!(map.remove(11).is_none());
    assert_eq!(map.len(), 1, "the refused removal changed nothing");
    assert_eq!(map.remove(10), Some((10..20, 'a')));
    assert!(map.is_empty());
    assert!(map.remove(10).is_none(), "a double removal is refused");
}

#[test]
fn overlapping_reports_every_intersecting_range_including_a_straddle() {
    let mut map: RangeMap<u64, char> = RangeMap::new();
    map.insert(0..10, 'a').expect("fits");
    map.insert(20..30, 'b').expect("fits");
    map.insert(40..50, 'c').expect("fits");
    let hit = |query: Range<u64>| -> alloc::vec::Vec<char> {
        map.overlapping(query).map(|(_, v)| *v).collect()
    };
    assert_eq!(hit(5..45), ['a', 'b', 'c'], "the straddle comes first");
    assert_eq!(hit(0..1), ['a']);
    assert_eq!(hit(9..21), ['a', 'b']);
    assert_eq!(hit(10..20), [] as [char; 0], "a gap intersects nothing");
    assert_eq!(hit(30..40), [] as [char; 0]);
    assert_eq!(hit(50..60), [] as [char; 0], "above every range");
    assert_eq!(
        hit(5..5),
        [] as [char; 0],
        "an empty query intersects nothing"
    );
    assert_eq!(
        hit(Range { start: 45, end: 5 }),
        [] as [char; 0],
        "an inverted query too"
    );
}

#[test]
fn ending_at_or_below_finds_the_highest_end_not_past_a_point() {
    let mut map: RangeMap<u64, char> = RangeMap::new();
    map.insert(0..10, 'a').expect("fits");
    map.insert(20..30, 'b').expect("fits");
    assert_eq!(
        map.ending_at_or_below(10),
        Some(0..10),
        "an exact end counts"
    );
    assert_eq!(map.ending_at_or_below(15), Some(0..10));
    // A range covering the point is excluded, and the predecessor answers.
    assert_eq!(map.ending_at_or_below(25), Some(0..10));
    assert_eq!(map.ending_at_or_below(30), Some(20..30));
    assert!(map.ending_at_or_below(5).is_none(), "nothing has ended yet");
    assert!(map.ending_at_or_below(0).is_none());
}

#[test]
fn first_free_is_first_fit_over_the_gaps() {
    let mut map: RangeMap<u64, char> = RangeMap::new();
    map.insert(10..20, 'a').expect("fits");
    map.insert(35..40, 'b').expect("fits");
    // Gaps of 10, 15, and 60: the lowest that fits wins, and a request too
    // large for one moves on to the next.
    assert_eq!(map.first_free(0..100, 10), Some(0));
    assert_eq!(map.first_free(0..100, 11), Some(20));
    assert_eq!(map.first_free(0..100, 15), Some(20));
    assert_eq!(map.first_free(0..100, 16), Some(40));
    assert_eq!(map.first_free(0..100, 60), Some(40));
    assert_eq!(map.first_free(0..100, 61), None, "no gap is that large");
    // The window bounds the answer at both ends.
    assert_eq!(map.first_free(25..100, 10), Some(25));
    assert_eq!(map.first_free(25..100, 11), Some(40));
    assert_eq!(map.first_free(0..45, 10), Some(0));
    assert_eq!(map.first_free(21..45, 14), Some(21));
    assert_eq!(
        map.first_free(21..45, 15),
        None,
        "the window bounds the tail gap"
    );
    assert_eq!(
        map.first_free(12..18, 1),
        None,
        "wholly inside a held range"
    );
    assert_eq!(map.first_free(0..100, 0), None, "nothing to place");
    assert_eq!(map.first_free(50..50, 1), None, "an empty window");
    let empty: RangeMap<u64, char> = RangeMap::new();
    assert_eq!(empty.first_free(7..100, 93), Some(7));
    assert_eq!(empty.first_free(7..100, 94), None);
}

#[test]
fn placement_walks_only_what_the_window_has_handed_out() {
    // The counter gate this replaces a bitmap scan for: placement cost is
    // the live range count, not the window's size. `place` walks exactly the
    // ranges intersecting the window, which `overlapping` counts — three,
    // whether the window spans a thousand slots or a billion.
    let window = 0..1_000_000_000usize;
    let mut map: RangeMap<usize, ()> = RangeMap::new();
    for start in [0, 100, 200] {
        map.insert(start..start + 10, ()).expect("disjoint");
    }
    assert_eq!(map.overlapping(window.clone()).count(), 3);
    assert_eq!(map.len(), 3, "one entry per run, none per slot");
    assert_eq!(map.place(window.clone(), 90, ()), Some(10..100));
    assert_eq!(map.place(window.clone(), 91, ()), Some(210..301));
    assert_eq!(map.overlapping(window).count(), 5);
}

#[test]
fn place_hands_out_a_run_and_reuses_what_is_released() {
    let mut map: RangeMap<usize, u32> = RangeMap::new();
    let window = 0..64;
    assert_eq!(map.place(window.clone(), 4, 1), Some(0..4));
    assert_eq!(map.place(window.clone(), 4, 2), Some(4..8));
    assert_eq!(map.remove(0), Some((0..4, 1)));
    assert_eq!(
        map.place(window.clone(), 4, 3),
        Some(0..4),
        "the released run is reused"
    );
    // A request the window cannot hold places nothing and leaves the map
    // exactly as it was.
    assert_eq!(map.place(window.clone(), 57, 4), None);
    assert_eq!(map.len(), 2);
    assert_eq!(map.place(window, 56, 4), Some(8..64));
}

#[test]
fn iteration_is_ascending_from_both_ends() {
    let mut map: RangeMap<u64, char> = RangeMap::new();
    for (start, value) in [(30u64, 'c'), (10, 'a'), (20, 'b')] {
        map.insert(start..start + 5, value).expect("disjoint");
    }
    assert_eq!(ranges(&map), [(10, 15), (20, 25), (30, 35)]);
    let forward: alloc::vec::Vec<char> = map.iter().map(|(_, v)| *v).collect();
    assert_eq!(forward, ['a', 'b', 'c']);
    let backward: alloc::vec::Vec<char> = map.iter().rev().map(|(_, v)| *v).collect();
    assert_eq!(backward, ['c', 'b', 'a']);
    assert_eq!(map.iter().len(), 3);
    // The borrowing `IntoIterator` is the same order.
    assert!((&map).into_iter().map(|(_, v)| *v).eq(forward));
    map.clear();
    assert!(map.is_empty() && map.iter().next().is_none());
}

#[test]
fn a_refused_insert_drops_the_value_it_could_not_place() {
    // The value is moved into a refused insert, so it must be dropped there
    // rather than leaked.
    use core::cell::Cell;

    struct Tracked<'a>(&'a Cell<i64>);
    impl Drop for Tracked<'_> {
        fn drop(&mut self) {
            self.0.set(self.0.get() - 1);
        }
    }

    let live = Cell::new(0i64);
    let mut map: RangeMap<u64, Tracked<'_>> = RangeMap::new();
    live.set(live.get() + 1);
    map.insert(0..10, Tracked(&live)).expect("fits");
    live.set(live.get() + 1);
    assert_eq!(map.insert(5..15, Tracked(&live)), Err(RangeError::Overlap));
    assert_eq!(live.get(), 1, "the refused value was dropped");
    drop(map);
    assert_eq!(live.get(), 0);
}

#[test]
fn an_error_states_which_refusal_it_was() {
    use alloc::string::ToString;

    assert_eq!(RangeError::Empty.to_string(), "the range covers nothing");
    assert_eq!(
        RangeError::Overlap.to_string(),
        "the range overlaps one already held"
    );
}
