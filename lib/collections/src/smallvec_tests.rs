//! Tests for [`SmallVec`]: the spill transition, and the allocation counter
//! the inline claim is gated on.

extern crate std;

use core::mem::size_of;

use crate::test_support::Counted;
use crate::{ArrayVec, SmallVec, TryReserveError};

#[test]
fn an_empty_vector_is_inline_and_reads_back_nothing() {
    let vec: SmallVec<u32, 4> = SmallVec::new();
    assert!(vec.is_empty());
    assert_eq!(vec.len(), 0);
    assert_eq!(vec.capacity(), 4);
    assert!(!vec.spilled());
    assert_eq!(vec.as_slice(), &[] as &[u32]);
}

/// The allocation counter: everything up to `N` is held inline, and exceeding
/// it costs one spill and correct behaviour rather than a refusal.
#[test]
fn the_inline_bound_is_the_only_allocation_free_window() {
    let mut vec: SmallVec<u32, 4> = SmallVec::new();
    for value in 0..4 {
        assert_eq!(vec.try_push(value), Ok(()));
        assert!(!vec.spilled(), "within the bound nothing is allocated");
    }
    assert_eq!(vec.try_push(4), Ok(()));
    assert!(vec.spilled());
    assert!(vec.capacity() >= 5);
    assert_eq!(
        vec.as_slice(),
        &[0, 1, 2, 3, 4],
        "the spill preserved order"
    );

    // A vector never returns inline, so no later operation can reallocate the
    // elements back into the array.
    while vec.pop().is_some() {
        assert!(vec.spilled());
    }
    assert!(vec.is_empty());
    assert!(vec.spilled());
}

#[test]
fn reserving_past_the_bound_spills_once() {
    let mut vec: SmallVec<u32, 8> = SmallVec::new();
    assert_eq!(vec.try_push(1), Ok(()));
    assert_eq!(vec.try_reserve(7), Ok(()), "still fits inline");
    assert!(!vec.spilled());
    assert_eq!(vec.try_reserve(8), Ok(()));
    assert!(vec.spilled());
    assert!(vec.capacity() >= 9);
    assert_eq!(vec.as_slice(), &[1]);

    let sized: SmallVec<u32, 4> = SmallVec::try_with_capacity(2).expect("inline");
    assert!(!sized.spilled());
    let spilled: SmallVec<u32, 4> = SmallVec::try_with_capacity(40).expect("heap");
    assert!(spilled.spilled());
    assert!(spilled.capacity() >= 40);
}

#[test]
fn an_unrepresentable_capacity_is_refused_rather_than_wrapped() {
    let mut vec: SmallVec<u64, 2> = SmallVec::new();
    assert_eq!(
        vec.try_reserve(usize::MAX),
        Err(TryReserveError::CapacityOverflow)
    );
    assert_eq!(
        vec.try_reserve(usize::MAX / 4),
        Err(TryReserveError::CapacityOverflow)
    );
    assert!(!vec.spilled(), "a refusal left the vector inline");
    assert_eq!(vec.try_push(1), Ok(()));
    assert_eq!(
        vec.try_reserve(usize::MAX),
        Err(TryReserveError::CapacityOverflow),
        "the length is counted into the request"
    );
}

/// Every positional operation must behave identically inline and spilled.
#[test]
fn the_two_storages_behave_alike() {
    for target in [3usize, 9] {
        let mut vec: SmallVec<u32, 4> = SmallVec::new();
        for value in 0..u32::try_from(target).expect("small") {
            assert_eq!(vec.try_push(value), Ok(()));
        }
        assert_eq!(vec.spilled(), target > 4);
        assert_eq!(vec.len(), target);

        assert_eq!(vec.remove(0), Some(0));
        assert_eq!(vec.remove(vec.len()), None, "one past the end");
        assert_eq!(vec.remove(usize::MAX), None);
        assert_eq!(vec.as_slice()[0], 1);

        let last = *vec.last().expect("non-empty");
        assert_eq!(vec.swap_remove(0), Some(1));
        assert_eq!(vec.as_slice()[0], last);
        assert_eq!(vec.swap_remove(vec.len()), None);

        vec.retain(|value| value % 2 == 0);
        assert!(vec.iter().all(|value| value % 2 == 0));

        vec.truncate(1);
        assert_eq!(vec.len(), 1);
        vec.clear();
        assert!(vec.is_empty());
        assert_eq!(vec.pop(), None);
    }
}

#[test]
fn extending_and_cloning_carry_the_elements_across() {
    let mut vec: SmallVec<u32, 4> = SmallVec::new();
    assert_eq!(vec.try_extend_from_slice(&[1, 2]), Ok(()));
    assert!(!vec.spilled());
    assert_eq!(vec.try_extend_from_slice(&[3, 4, 5, 6]), Ok(()));
    assert!(vec.spilled());
    assert_eq!(vec.as_slice(), &[1, 2, 3, 4, 5, 6]);

    let copy = vec.try_clone().expect("room");
    assert_eq!(copy, vec);
    assert!(copy.spilled(), "a copy that does not fit inline spills too");

    let small: SmallVec<u32, 4> = SmallVec::try_from(&[7, 8][..]).expect("fits");
    assert_eq!(small.as_slice(), &[7, 8]);
    assert!(!small.try_clone().expect("room").spilled());
}

#[test]
fn the_deref_supplies_slice_behaviour_on_both_storages() {
    let mut vec: SmallVec<u32, 2> = SmallVec::new();
    assert_eq!(vec.try_extend_from_slice(&[5, 1, 4, 2]), Ok(()));
    vec.sort_unstable();
    assert_eq!(&*vec, &[1, 2, 4, 5]);
    assert_eq!(vec.binary_search(&4), Ok(2));
    for value in &mut vec {
        *value *= 2;
    }
    assert_eq!(&*vec, &[2, 4, 8, 10]);
}

#[test]
fn an_owning_iterator_yields_from_either_end_on_both_storages() {
    for capacity_fits in [true, false] {
        let mut vec: SmallVec<u32, 4> = SmallVec::new();
        let count = if capacity_fits { 4 } else { 6 };
        assert_eq!(
            vec.try_extend_from_slice(&(0..count).collect::<std::vec::Vec<_>>()),
            Ok(())
        );
        assert_eq!(vec.spilled(), !capacity_fits);
        let mut iter = vec.into_iter();
        assert_eq!(iter.len(), usize::try_from(count).expect("small"));
        assert_eq!(iter.next(), Some(0));
        assert_eq!(iter.next_back(), Some(count - 1));
        assert_eq!(iter.count(), usize::try_from(count).expect("small") - 2);
    }
}

/// Every element the vector ever took must be dropped exactly once, including
/// across the spill that moves them all to the heap.
#[test]
fn every_element_is_dropped_exactly_once_across_the_spill() {
    let drops = Counted::counter();
    {
        let mut vec: SmallVec<Counted, 3> = SmallVec::new();
        for _ in 0..3 {
            assert!(vec.try_push(Counted::new(&drops)).is_ok());
        }
        assert!(!vec.spilled());
        assert!(vec.try_push(Counted::new(&drops)).is_ok());
        assert!(vec.spilled());
        assert_eq!(drops.get(), 0, "the spill moves values, never drops them");

        drop(vec.pop());
        drop(vec.remove(0));
        drop(vec.swap_remove(0));
        assert_eq!(drops.get(), 3);
        vec.retain(|_| false);
        assert_eq!(drops.get(), 4);
        for _ in 0..2 {
            assert!(vec.try_push(Counted::new(&drops)).is_ok());
        }
    }
    assert_eq!(drops.get(), 6);
}

/// The footprint budget: the larger of the two arms, plus at most one word to
/// say which arm is live. A tighter layout (the compiler finding a niche in the
/// heap arm) is a saving, so the gate is a ceiling.
#[test]
fn the_inline_footprint_holds_no_heap_pointer() {
    assert!(
        size_of::<SmallVec<u64, 8>>() <= size_of::<ArrayVec<u64, 8>>() + size_of::<usize>(),
        "the inline array dominates at this bound"
    );
    assert!(
        size_of::<SmallVec<u64, 1>>() <= size_of::<alloc::vec::Vec<u64>>() + size_of::<usize>(),
        "below the heap arm's own width the spill representation dominates"
    );
}
