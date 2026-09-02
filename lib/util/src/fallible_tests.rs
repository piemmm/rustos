//! Tests for the shared fallible allocation helpers.

use super::{collected, filled, grow_to, reserve};
use alloc::vec::Vec;

/// A count whose `T`-sized layout cannot be reserved on any target: the
/// byte size overflows, so the refusal is the allocator's answer rather
/// than a property of the host's free memory.
const UNRESERVABLE: usize = usize::MAX / 2;

#[test]
fn a_refused_reservation_is_answered_not_aborted() {
    assert_eq!(filled::<u32>(UNRESERVABLE, 0), None);
    assert_eq!(collected(UNRESERVABLE, core::iter::repeat(0_u32)), None);
    let mut scratch: Vec<u32> = Vec::new();
    assert!(!grow_to(&mut scratch, UNRESERVABLE, 0));
    assert!(scratch.is_empty(), "a refused growth leaves the scratch");
    let mut buffer: Vec<u32> = Vec::new();
    assert!(!reserve(&mut buffer, UNRESERVABLE));
}

#[test]
fn a_granted_reservation_holds_exactly_what_was_asked_for() {
    assert_eq!(filled(3, 7_u8), Some(alloc::vec![7, 7, 7]));
    assert_eq!(
        collected(2, [1_u8, 2, 3].into_iter()),
        Some(alloc::vec![1, 2])
    );
    let mut scratch = alloc::vec![1_u8, 2];
    assert!(grow_to(&mut scratch, 4, 9));
    assert_eq!(scratch, alloc::vec![1, 2, 9, 9]);
    assert!(
        grow_to(&mut scratch, 1, 9),
        "a scratch already long enough is left alone"
    );
    assert_eq!(scratch.len(), 4);
}
