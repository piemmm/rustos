//! Tests for [`RingBuf`]: both ends, the wrap, the bulk slice paths, the
//! secret-hygiene scrubs, and the footprint counter.

extern crate std;

use core::mem::size_of;

use crate::{CapacityError, RingBuf, SecretRing};
use tairix_fuzzseed::Counted;

#[test]
fn an_empty_ring_reads_back_nothing() {
    let ring: RingBuf<u32, 4> = RingBuf::new();
    assert!(ring.is_empty());
    assert!(!ring.is_full());
    assert_eq!(ring.len(), 0);
    assert_eq!(ring.capacity(), 4);
    assert_eq!(ring.remaining_capacity(), 4);
    assert_eq!(ring.front(), None);
    assert_eq!(ring.back(), None);
    assert_eq!(ring.get(0), None);
    assert_eq!(ring.iter().count(), 0);
}

#[test]
fn pushes_and_pops_reach_both_ends() {
    let mut ring: RingBuf<u32, 4> = RingBuf::new();
    assert_eq!(ring.try_push_back(2), Ok(()));
    assert_eq!(ring.try_push_back(3), Ok(()));
    assert_eq!(ring.try_push_front(1), Ok(()));
    assert_eq!(ring.try_push_front(0), Ok(()));
    assert!(ring.is_full());
    assert_eq!(
        ring.iter().copied().collect::<std::vec::Vec<_>>(),
        [0, 1, 2, 3]
    );
    assert_eq!(ring.front(), Some(&0));
    assert_eq!(ring.back(), Some(&3));

    assert_eq!(ring.try_push_back(9), Err(CapacityError::new(9)));
    assert_eq!(ring.try_push_front(9), Err(CapacityError::new(9)));
    assert_eq!(ring.len(), 4, "a refusal changed nothing");

    assert_eq!(ring.pop_front(), Some(0));
    assert_eq!(ring.pop_back(), Some(3));
    assert_eq!(ring.iter().copied().collect::<std::vec::Vec<_>>(), [1, 2]);
    assert_eq!(ring.pop_front(), Some(1));
    assert_eq!(ring.pop_back(), Some(2));
    assert_eq!(ring.pop_front(), None);
    assert_eq!(ring.pop_back(), None);
}

/// Head and tail must keep advancing past the physical end of the slot array
/// without disordering or losing an element.
#[test]
fn the_window_wraps_the_physical_end_of_the_array() {
    let mut ring: RingBuf<u32, 5> = RingBuf::new();
    for value in 0..3 {
        assert_eq!(ring.try_push_back(value), Ok(()));
    }
    // Push one and pop one per round, so three elements stay resident while
    // the window walks the array many times over.
    for round in 3..60u32 {
        assert_eq!(ring.try_push_back(round), Ok(()));
        assert_eq!(ring.pop_front(), Some(round - 3));
        assert_eq!(ring.len(), 3);
        assert_eq!(
            ring.iter().copied().collect::<std::vec::Vec<_>>(),
            [round - 2, round - 1, round]
        );
        // Offsets are counted from the front, across the wrap.
        for offset in 0..3u32 {
            assert_eq!(ring.get(offset as usize), Some(&(round - 2 + offset)));
        }
        assert_eq!(ring.get(3), None);
    }
}

#[test]
fn the_overwriting_push_evicts_the_front_and_returns_it() {
    let mut ring: RingBuf<u32, 3> = RingBuf::new();
    for value in 0..3 {
        assert_eq!(ring.push_back_overwrite(value), None);
    }
    assert_eq!(ring.push_back_overwrite(3), Some(0));
    assert_eq!(ring.push_back_overwrite(4), Some(1));
    assert_eq!(
        ring.iter().copied().collect::<std::vec::Vec<_>>(),
        [2, 3, 4]
    );
    assert_eq!(ring.len(), 3);
}

/// A zero-capacity ring is a legal, degenerate configuration: it stores
/// nothing and refuses everything rather than indexing an empty array.
#[test]
fn a_zero_capacity_ring_stores_nothing_and_never_panics() {
    let mut ring: RingBuf<u32, 0> = RingBuf::new();
    assert!(ring.is_empty());
    assert!(ring.is_full());
    assert_eq!(ring.capacity(), 0);
    assert_eq!(ring.try_push_back(1), Err(CapacityError::new(1)));
    assert_eq!(ring.try_push_front(1), Err(CapacityError::new(1)));
    assert_eq!(ring.push_back_overwrite(1), None);
    assert_eq!(ring.pop_front(), None);
    assert_eq!(ring.pop_back(), None);
    assert_eq!(ring.get(0), None);
    assert_eq!(ring.discard_front(4), 0);
    assert_eq!(ring.push_slice(&[1, 2]), 0);
    assert_eq!(ring.peek_slice(0, &mut [0u32; 2]), 0);
}

#[test]
fn a_bulk_push_is_short_when_the_ring_fills() {
    let mut ring: RingBuf<u8, 6> = RingBuf::new();
    assert_eq!(ring.push_slice(b"abcd"), 4);
    // Only two slots are left, so a four-byte push takes two and the producer
    // retries the rest.
    assert_eq!(ring.push_slice(b"efgh"), 2);
    assert!(ring.is_full());
    assert_eq!(
        ring.iter().copied().collect::<std::vec::Vec<_>>(),
        *b"abcdef"
    );
    assert_eq!(ring.push_slice(b"z"), 0);
}

#[test]
fn a_whole_slice_push_is_all_or_nothing() {
    let mut ring: RingBuf<u8, 6> = RingBuf::new();
    assert_eq!(ring.try_push_slice(b"abcd"), Ok(()));
    assert_eq!(ring.try_push_slice(b"efgh"), Err(CapacityError::new(())));
    assert_eq!(ring.len(), 4, "the refusal stored no prefix");
    assert_eq!(ring.try_push_slice(b"ef"), Ok(()));
    assert_eq!(
        ring.iter().copied().collect::<std::vec::Vec<_>>(),
        *b"abcdef"
    );
}

/// The bulk paths must copy correctly when the live window, the source, or
/// both straddle the physical end of the array.
#[test]
fn the_bulk_paths_cross_the_wrap_intact() {
    let mut ring: RingBuf<u8, 8> = RingBuf::new();
    let mut scratch = [0u8; 8];
    for start in 0..8usize {
        // Walk the window's origin to `start`, then exercise a wrapped push
        // and a wrapped drain from there.
        let mut prime: RingBuf<u8, 8> = RingBuf::new();
        core::mem::swap(&mut ring, &mut prime);
        assert_eq!(ring.push_slice(&[0u8; 8][..start]), start);
        assert_eq!(ring.discard_front(start), start);

        assert_eq!(ring.try_push_slice(b"abcdefg"), Ok(()));
        // A peek reads without consuming, at any offset.
        assert_eq!(ring.peek_slice(0, &mut scratch[..7]), 7);
        assert_eq!(&scratch[..7], b"abcdefg");
        assert_eq!(ring.peek_slice(3, &mut scratch[..4]), 4);
        assert_eq!(&scratch[..4], b"defg");
        assert_eq!(ring.peek_slice(7, &mut scratch), 0, "at the end");
        assert_eq!(ring.peek_slice(9, &mut scratch), 0, "past the end");
        assert_eq!(ring.len(), 7, "a peek consumes nothing");

        // A short output buffer takes a prefix; the drain then takes the rest.
        assert_eq!(ring.pop_slice(&mut scratch[..3]), 3);
        assert_eq!(&scratch[..3], b"abc");
        assert_eq!(ring.pop_slice(&mut scratch), 4);
        assert_eq!(&scratch[..4], b"defg");
        assert!(ring.is_empty());
    }
}

#[test]
fn discarding_from_the_front_drops_exactly_what_it_removes() {
    let drops = Counted::counter();
    let mut ring: RingBuf<Counted, 6> = RingBuf::new();
    for _ in 0..6 {
        assert!(ring.try_push_back(Counted::new(&drops)).is_ok());
    }
    assert_eq!(ring.discard_front(2), 2);
    assert_eq!(drops.get(), 2);
    assert_eq!(ring.len(), 4);
    // Refill so the window straddles the array end, then discard across it.
    for _ in 0..2 {
        assert!(ring.try_push_back(Counted::new(&drops)).is_ok());
    }
    assert_eq!(ring.discard_front(99), 6, "clamped to the live count");
    assert_eq!(drops.get(), 8);
    assert!(ring.is_empty());
}

/// Every element the ring ever took must be dropped exactly once, whichever
/// path removed it — including the eviction an overwriting push performs.
#[test]
fn every_element_is_dropped_exactly_once() {
    let drops = Counted::counter();
    {
        let mut ring: RingBuf<Counted, 4> = RingBuf::new();
        for _ in 0..4 {
            assert!(ring.try_push_back(Counted::new(&drops)).is_ok());
        }
        let refused = ring
            .try_push_back(Counted::new(&drops))
            .expect_err("the ring is full");
        assert_eq!(drops.get(), 0, "a refusal drops nothing");
        drop(refused.into_value());
        assert_eq!(drops.get(), 1);

        drop(ring.pop_front());
        drop(ring.pop_back());
        assert_eq!(drops.get(), 3);

        // The eviction hands the displaced element back rather than dropping
        // it, so the caller decides.
        assert!(ring.try_push_back(Counted::new(&drops)).is_ok());
        assert!(ring.try_push_back(Counted::new(&drops)).is_ok());
        let evicted = ring.push_back_overwrite(Counted::new(&drops));
        assert!(evicted.is_some());
        assert_eq!(drops.get(), 3);
        drop(evicted);
        assert_eq!(drops.get(), 4);

        ring.clear();
        assert_eq!(drops.get(), 8, "the four still queued");
        for _ in 0..3 {
            assert!(ring.try_push_back(Counted::new(&drops)).is_ok());
        }
    }
    // Four, one refused, three, one overwriting, three more: eleven values.
    assert_eq!(drops.get(), 11);
}

/// A ring a credential transits must retain no copy of it once the consumer
/// has taken it, so every drain blanks the slot it vacates.
#[test]
fn a_secret_ring_leaves_nothing_in_the_slots_it_vacates() {
    let mut ring: SecretRing<u8, 4> = SecretRing::new(0);
    // The whole store is blank before anything is ever pushed, which is what
    // makes reading it back sound at all.
    assert_eq!(ring.backing_store(), b"\0\0\0\0");

    assert_eq!(ring.push_slice(b"pass"), 4);
    assert_eq!(ring.pop_front(), Some(b'p'));
    assert_eq!(
        ring.backing_store(),
        b"\0ass",
        "the drained slot was blanked"
    );
    assert_eq!(ring.discard_front(2), 2);
    assert_eq!(ring.backing_store(), b"\0\0\0s");
    assert_eq!(ring.len(), 1, "the last byte is still queued");

    // A purge reaches what is still queued and the residue beyond the window
    // alike, which is what a change of holder needs.
    ring.purge();
    assert!(ring.is_empty());
    assert_eq!(ring.backing_store(), b"\0\0\0\0");

    // Reads reach through unchanged, and the ring is fully usable afterwards.
    assert_eq!(ring.push_slice(b"ok"), 2);
    assert_eq!(ring.iter().copied().collect::<std::vec::Vec<_>>(), *b"ok");
    assert_eq!(ring.front(), Some(&b'o'));
    assert_eq!(ring.remaining_capacity(), 2);

    let mut empty: SecretRing<u8, 4> = SecretRing::new(0);
    assert_eq!(empty.pop_front(), None);
    assert_eq!(empty.discard_front(2), 0);
    assert_eq!(empty.try_push_back(b'z'), Ok(()));
    assert_eq!(empty.backing_store(), b"z\0\0\0");
}

/// A wider element scrubs whole records, and the eviction path reaches the
/// slot it displaced rather than only the one a caller drained.
#[test]
fn a_secret_ring_of_records_scrubs_a_displaced_record() {
    let mut ring: SecretRing<[u8; 3], 2> = SecretRing::new([0; 3]);
    assert_eq!(ring.try_push_back(*b"abc"), Ok(()));
    assert_eq!(ring.try_push_back(*b"def"), Ok(()));
    assert!(ring.is_full());
    assert_eq!(
        ring.try_push_back(*b"ghi"),
        Err(CapacityError::new(*b"ghi"))
    );

    // What a producer that must displace the oldest record does: discard it,
    // which scrubs its slot, then push into the room that made.
    assert_eq!(ring.discard_front(1), 1);
    assert_eq!(ring.backing_store(), &[*b"\0\0\0", *b"def"]);
    assert_eq!(ring.try_push_back(*b"ghi"), Ok(()));
    assert_eq!(
        ring.iter().copied().collect::<std::vec::Vec<_>>(),
        [*b"def", *b"ghi"]
    );

    assert_eq!(ring.pop_front(), Some(*b"def"));
    assert_eq!(ring.backing_store(), &[*b"ghi", *b"\0\0\0"]);
}

#[test]
fn a_clone_preserves_order_and_is_independent() {
    let mut ring: RingBuf<u32, 4> = RingBuf::new();
    for value in 0..6 {
        ring.push_back_overwrite(value);
    }
    let mut copy = ring.clone();
    assert_eq!(copy, ring);
    assert_eq!(
        copy.iter().copied().collect::<std::vec::Vec<_>>(),
        [2, 3, 4, 5]
    );
    assert_eq!(copy.pop_front(), Some(2));
    assert_ne!(copy, ring);
    assert_eq!(ring.len(), 4);
}

#[test]
fn mutable_access_reaches_the_element_at_an_offset() {
    let mut ring: RingBuf<u32, 4> = RingBuf::new();
    for value in 0..6 {
        ring.push_back_overwrite(value);
    }
    *ring.get_mut(1).expect("in range") = 99;
    assert_eq!(
        ring.iter().copied().collect::<std::vec::Vec<_>>(),
        [2, 99, 4, 5]
    );
    assert_eq!(ring.get_mut(4), None);
}

/// The footprint gate: the slots plus a head and a length, with no heap block
/// and no per-element overhead.
#[test]
fn the_footprint_is_the_slots_plus_two_indices() {
    assert_eq!(size_of::<RingBuf<u8, 256>>(), 256 + 2 * size_of::<usize>());
    assert_eq!(
        size_of::<RingBuf<u32, 64>>(),
        64 * size_of::<u32>() + 2 * size_of::<usize>()
    );
}
