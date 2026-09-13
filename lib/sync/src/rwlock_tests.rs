//! The state-word transitions, exercised at the boundaries the live lock
//! cannot reach.
//!
//! Saturating either count needs billions of concurrent guards, so the
//! transitions are pure functions of the word precisely so their refusals can
//! be driven directly. These tests are the only thing standing behind the
//! saturation and underflow guards.

use super::{
    pending_registered, pending_writers, reader_acquired, reader_count, writer_acquired,
    writer_held, MAX_PENDING, MAX_READERS, PENDING_MASK, PENDING_ONE, READER_MASK, READER_ONE,
    WRITER_BIT,
};

/// The three fields must tile the word exactly. If they overlap, one field's
/// increment silently edits another; if they leave a gap, the masks below no
/// longer describe the counts the lock reads back.
#[test]
fn the_fields_tile_the_state_word_without_overlap() {
    assert_eq!(WRITER_BIT & READER_MASK, 0);
    assert_eq!(WRITER_BIT & PENDING_MASK, 0);
    assert_eq!(READER_MASK & PENDING_MASK, 0);
    assert_eq!(WRITER_BIT | READER_MASK | PENDING_MASK, usize::MAX);
}

#[test]
fn the_field_accessors_round_trip() {
    let state = WRITER_BIT | (3 * READER_ONE) | (5 * PENDING_ONE);
    assert!(writer_held(state));
    assert_eq!(reader_count(state), 3);
    assert_eq!(pending_writers(state), 5);
}

#[test]
fn a_reader_is_admitted_only_when_no_writer_holds_or_waits() {
    assert_eq!(reader_acquired(0), Some(READER_ONE));
    assert_eq!(reader_acquired(READER_ONE), Some(2 * READER_ONE));
    // Writer preference: a held *or* merely pending writer turns readers away.
    assert_eq!(reader_acquired(WRITER_BIT), None);
    assert_eq!(reader_acquired(PENDING_ONE), None);
}

/// The reader field is full: admitting one more would carry into the pending
/// count and read back as a queue of writers that does not exist.
#[test]
fn a_reader_is_refused_rather_than_wrapped_when_the_count_is_full() {
    let full = MAX_READERS * READER_ONE;
    assert_eq!(reader_count(full), MAX_READERS);
    assert_eq!(reader_acquired(full), None);
    // One short of full still admits, and lands exactly on full.
    let nearly = (MAX_READERS - 1) * READER_ONE;
    assert_eq!(reader_acquired(nearly), Some(full));
}

#[test]
fn an_intent_registers_until_the_pending_field_is_full() {
    assert_eq!(pending_registered(0), Some(PENDING_ONE));
    assert_eq!(
        pending_registered(WRITER_BIT),
        Some(WRITER_BIT | PENDING_ONE)
    );

    let full = MAX_PENDING * PENDING_ONE;
    assert_eq!(pending_writers(full), MAX_PENDING);
    assert_eq!(pending_registered(full), None);
    let nearly = (MAX_PENDING - 1) * PENDING_ONE;
    assert_eq!(pending_registered(nearly), Some(full));
}

/// The refusal has to come *before* the increment. An add that wraps first and
/// checks afterwards has already published a state whose pending count reads
/// zero, and for that window every reader walks straight past a full queue of
/// writers — the fairness invariant inverted at exactly the moment it matters
/// most.
#[test]
fn a_full_pending_field_is_never_wrapped_through_zero() {
    let full = MAX_PENDING * PENDING_ONE;
    assert_eq!(pending_registered(full), None);
    // What the discarded increment would have produced, had it been performed
    // before the check: the count back at zero, and the readers' own field
    // untouched so nothing else flags the corruption.
    let wrapped = full.wrapping_add(PENDING_ONE);
    assert_eq!(pending_writers(wrapped), 0);
    assert_eq!(reader_acquired(wrapped), Some(READER_ONE));
}

#[test]
fn a_writer_takes_the_lock_only_against_an_idle_state() {
    let registered = PENDING_ONE;
    assert_eq!(writer_acquired(registered), Some(WRITER_BIT));
    // A second registered writer keeps its intent while the first takes it.
    assert_eq!(
        writer_acquired(2 * PENDING_ONE),
        Some(WRITER_BIT | PENDING_ONE)
    );
    // Any reader, or the bit already set, means wait.
    assert_eq!(writer_acquired(registered | READER_ONE), None);
    assert_eq!(writer_acquired(registered | WRITER_BIT), None);
}

/// Taking the lock withdraws the taker's own intent, so an unregistered caller
/// would borrow one it never lodged. The borrow wraps the pending count to its
/// maximum, and because readers defer to a pending writer the lock then turns
/// every reader away for ever, waiting on a queue of writers that does not
/// exist — a permanent wedge, not a transient miscount.
#[test]
fn an_unregistered_writer_is_refused_rather_than_underflowed() {
    assert_eq!(pending_writers(0), 0);
    assert_eq!(writer_acquired(0), None);
    // What the withdrawal would have produced without the guard.
    let underflowed = 0usize.wrapping_sub(PENDING_ONE) | WRITER_BIT;
    assert_eq!(pending_writers(underflowed), MAX_PENDING);
    assert_eq!(reader_acquired(underflowed & !WRITER_BIT), None);
}

/// Every transition is a function of the word alone, so composing them must
/// return the lock to rest: readers in and out, a writer registered, served
/// and released, leaves nothing behind.
#[test]
fn the_transitions_compose_back_to_an_idle_state() {
    let mut state = 0usize;
    for _ in 0..3 {
        state = reader_acquired(state).expect("an idle lock admits readers");
    }
    assert_eq!(reader_count(state), 3);
    // Readers drain the way the guard's `Drop` spells it.
    for _ in 0..3 {
        state -= READER_ONE;
    }
    assert_eq!(state, 0);

    state = pending_registered(state).expect("an idle lock accepts an intent");
    state = writer_acquired(state).expect("no readers, so the writer is served");
    assert!(writer_held(state));
    assert_eq!(pending_writers(state), 0);
    // And the write guard's `Drop`.
    state &= !WRITER_BIT;
    assert_eq!(state, 0);
}
