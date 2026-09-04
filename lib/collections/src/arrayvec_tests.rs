//! Tests for [`ArrayVec`], including the footprint counter its zero-overhead
//! claim is gated on.

extern crate std;

use core::mem::size_of;

use crate::test_support::Counted;
use crate::{ArrayVec, CapacityError};

#[test]
fn an_empty_vector_reads_back_nothing() {
    let vec: ArrayVec<u32, 4> = ArrayVec::new();
    assert_eq!(vec.len(), 0);
    assert!(vec.is_empty());
    assert!(!vec.is_full());
    assert_eq!(vec.capacity(), 4);
    assert_eq!(vec.remaining_capacity(), 4);
    assert_eq!(vec.as_slice(), &[] as &[u32]);
    assert_eq!(vec.first(), None);
}

#[test]
fn a_full_vector_refuses_and_hands_the_value_back() {
    let mut vec: ArrayVec<u32, 3> = ArrayVec::new();
    for value in 0..3 {
        assert_eq!(vec.try_push(value), Ok(()));
    }
    assert!(vec.is_full());
    assert_eq!(vec.try_push(99), Err(CapacityError::new(99)));
    // The refusal changed nothing and the value survived it.
    assert_eq!(vec.as_slice(), &[0, 1, 2]);
    let returned = vec.try_push(99).expect_err("full").into_value();
    assert_eq!(returned, 99);
}

#[test]
fn pop_and_truncate_shorten_the_live_prefix() {
    let mut vec: ArrayVec<u32, 8> = ArrayVec::new();
    for value in 0..6 {
        assert_eq!(vec.try_push(value), Ok(()));
    }
    assert_eq!(vec.pop(), Some(5));
    vec.truncate(3);
    assert_eq!(vec.as_slice(), &[0, 1, 2]);
    // A truncation at or above the length changes nothing.
    vec.truncate(9);
    assert_eq!(vec.as_slice(), &[0, 1, 2]);
    vec.clear();
    assert!(vec.is_empty());
    assert_eq!(vec.pop(), None);
}

#[test]
fn remove_shifts_the_tail_and_refuses_an_index_past_the_end() {
    let mut vec: ArrayVec<u32, 8> = ArrayVec::new();
    for value in 0..5 {
        assert_eq!(vec.try_push(value), Ok(()));
    }
    assert_eq!(vec.remove(1), Some(1));
    assert_eq!(vec.as_slice(), &[0, 2, 3, 4]);
    assert_eq!(vec.remove(3), Some(4));
    assert_eq!(vec.as_slice(), &[0, 2, 3]);
    assert_eq!(vec.remove(3), None, "one past the end");
    assert_eq!(vec.remove(usize::MAX), None);
    assert_eq!(vec.as_slice(), &[0, 2, 3]);
}

#[test]
fn swap_remove_moves_the_last_element_into_the_hole() {
    let mut vec: ArrayVec<u32, 8> = ArrayVec::new();
    for value in 0..4 {
        assert_eq!(vec.try_push(value), Ok(()));
    }
    assert_eq!(vec.swap_remove(0), Some(0));
    assert_eq!(vec.as_slice(), &[3, 1, 2]);
    assert_eq!(
        vec.swap_remove(2),
        Some(2),
        "the last element is its own swap"
    );
    assert_eq!(vec.as_slice(), &[3, 1]);
    assert_eq!(vec.swap_remove(2), None);
}

#[test]
fn retain_keeps_the_accepted_elements_in_order() {
    let mut vec: ArrayVec<u32, 16> = ArrayVec::new();
    for value in 0..10 {
        assert_eq!(vec.try_push(value), Ok(()));
    }
    vec.retain(|value| value % 3 == 0);
    assert_eq!(vec.as_slice(), &[0, 3, 6, 9]);
    vec.retain(|_| false);
    assert!(vec.is_empty());
}

#[test]
fn extending_from_a_slice_is_all_or_nothing() {
    let mut vec: ArrayVec<u32, 4> = ArrayVec::new();
    assert_eq!(vec.try_extend_from_slice(&[1, 2]), Ok(()));
    // Three more would overrun the remaining two slots, so none are taken.
    assert_eq!(
        vec.try_extend_from_slice(&[3, 4, 5]),
        Err(CapacityError::new(()))
    );
    assert_eq!(vec.as_slice(), &[1, 2]);
    assert_eq!(vec.try_extend_from_slice(&[3, 4]), Ok(()));
    assert_eq!(vec.as_slice(), &[1, 2, 3, 4]);

    let built: ArrayVec<u32, 4> = ArrayVec::try_from(&[9, 8][..]).expect("fits");
    assert_eq!(built.as_slice(), &[9, 8]);
    assert!(ArrayVec::<u32, 1>::try_from(&[9, 8][..]).is_err());
}

#[test]
fn the_deref_supplies_slice_behaviour() {
    let mut vec: ArrayVec<u32, 8> = ArrayVec::new();
    assert_eq!(vec.try_extend_from_slice(&[5, 1, 4, 2]), Ok(()));
    vec.sort_unstable();
    assert_eq!(&*vec, &[1, 2, 4, 5]);
    assert_eq!(vec.binary_search(&4), Ok(2));
    assert_eq!(vec.iter().sum::<u32>(), 12);
    for value in &mut vec {
        *value *= 2;
    }
    assert_eq!(&*vec, &[2, 4, 8, 10]);
}

#[test]
fn a_clone_is_independent_of_its_source() {
    let mut vec: ArrayVec<u32, 4> = ArrayVec::new();
    assert_eq!(vec.try_extend_from_slice(&[1, 2, 3]), Ok(()));
    let mut copy = vec.clone();
    assert_eq!(copy, vec);
    assert_eq!(copy.pop(), Some(3));
    assert_eq!(vec.as_slice(), &[1, 2, 3]);
    assert_ne!(copy, vec);
}

#[test]
fn an_owning_iterator_yields_from_either_end() {
    let mut vec: ArrayVec<u32, 8> = ArrayVec::new();
    assert_eq!(vec.try_extend_from_slice(&[1, 2, 3, 4]), Ok(()));
    let mut iter = vec.into_iter();
    assert_eq!(iter.len(), 4);
    assert_eq!(iter.next(), Some(1));
    assert_eq!(iter.next_back(), Some(4));
    assert_eq!(iter.len(), 2);
    assert_eq!(iter.next(), Some(2));
    assert_eq!(iter.next_back(), Some(3));
    assert_eq!(iter.next(), None);
    assert_eq!(iter.next_back(), None);
}

/// Every element the vector ever took must be dropped exactly once, whichever
/// path removed it.
#[test]
fn every_element_is_dropped_exactly_once() {
    let drops = Counted::counter();
    {
        let mut vec: ArrayVec<Counted, 8> = ArrayVec::new();
        for _ in 0..8 {
            assert!(vec.try_push(Counted::new(&drops)).is_ok());
        }
        // A refused push drops nothing: the value comes back out.
        let refused = vec.try_push(Counted::new(&drops)).expect_err("full");
        assert_eq!(drops.get(), 0);
        drop(refused.into_value());
        assert_eq!(drops.get(), 1);

        drop(vec.pop());
        assert_eq!(drops.get(), 2);
        drop(vec.remove(0));
        assert_eq!(drops.get(), 3);
        drop(vec.swap_remove(0));
        assert_eq!(drops.get(), 4);
        vec.truncate(3);
        assert_eq!(drops.get(), 6);
        vec.retain(|_| false);
        assert_eq!(drops.get(), 9);

        for _ in 0..4 {
            assert!(vec.try_push(Counted::new(&drops)).is_ok());
        }
        assert_eq!(drops.get(), 9);
    }
    // Eight pushed, one refused, four pushed again: thirteen values, one drop
    // each.
    assert_eq!(drops.get(), 13);
}

#[test]
fn an_abandoned_owning_iterator_drops_what_it_did_not_yield() {
    let drops = Counted::counter();
    let mut vec: ArrayVec<Counted, 8> = ArrayVec::new();
    for _ in 0..8 {
        assert!(vec.try_push(Counted::new(&drops)).is_ok());
    }
    let mut iter = vec.into_iter();
    drop(iter.next().expect("an element"));
    drop(iter.next_back().expect("an element"));
    assert_eq!(drops.get(), 2);
    drop(iter);
    assert_eq!(
        drops.get(),
        8,
        "the six never yielded were dropped once each"
    );
}

/// A predicate that unwinds must leave the vector consistent and lose nothing
/// it had not already rejected: the tail the sweep never reached shifts down
/// over the gap and stays the vector's. Only the host build can unwind at all
/// (the kernel aborts), so this guards the test and Miri configurations.
#[test]
fn a_retain_predicate_that_unwinds_neither_leaks_nor_double_drops() {
    let drops = Counted::counter();
    let mut vec: ArrayVec<Counted, 8> = ArrayVec::new();
    for _ in 0..8 {
        assert!(vec.try_push(Counted::new(&drops)).is_ok());
    }
    // Reject the first and third elements, then unwind while testing the
    // fifth, so the guard has a real gap to shift the tail over.
    let mut seen = 0;
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        vec.retain(|_| {
            seen += 1;
            assert!(seen < 5, "deliberate unwind part-way through the sweep");
            seen % 2 == 0
        });
    }));
    assert!(outcome.is_err(), "the predicate unwound");
    assert_eq!(
        drops.get(),
        2,
        "only the two rejected elements were dropped"
    );
    assert_eq!(
        vec.len(),
        6,
        "the two survivors plus the four the sweep never reached"
    );
    drop(vec);
    assert_eq!(drops.get(), 8, "every element accounted for, none twice");
}

/// The footprint gate: an inline vector is its elements plus one length, with
/// no heap block and no per-element overhead.
#[test]
fn the_footprint_is_the_elements_plus_one_length() {
    assert_eq!(size_of::<ArrayVec<u8, 64>>(), 64 + size_of::<usize>());
    assert_eq!(
        size_of::<ArrayVec<u64, 16>>(),
        16 * size_of::<u64>() + size_of::<usize>()
    );
}
