//! Host tests for the taskbar clock's reading, spelling, and tick.
//!
//! All of it is a pure function of a wall-clock reading and a monotonic
//! instant, so none of it needs a kernel: what an unset clock draws, how a
//! reading is spelled, when the label next goes stale, and that an idle
//! desktop arms no deadline it does not owe.

use tairix_abi::time::{Time64, WallClockReading, WallTimeState};

use super::{spell, SessionClock, UNSET_LABEL};
use crate::switchuser::NO_DEADLINE_NS;

/// Nanoseconds in one second, for readable deadlines.
const SEC: u64 = 1_000_000_000;

/// A trusted reading at `secs` past the epoch, `nanos` into that second.
fn at(secs: i64, nanos: u32) -> WallClockReading {
    WallClockReading::new(
        Time64::new(secs, nanos).expect("a canonical instant"),
        WallTimeState::Trusted,
    )
}

#[test]
fn a_reading_spells_the_hour_and_minute_zero_padded() {
    // 2024-02-29 13:46:07 UTC — the shared civil breakdown, spelled to the
    // minute the bar shows.
    assert_eq!(spell(at(1_709_214_367, 0)), "13:46");
    // Midnight pads both fields rather than collapsing to "0:0".
    assert_eq!(spell(at(0, 0)), "00:00");
    // The last minute of the day.
    assert_eq!(spell(at(86_399, 0)), "23:59");
    // Before the epoch counts backwards correctly rather than wrapping: one
    // second before is 23:59 of the previous day.
    assert_eq!(spell(at(-1, 0)), "23:59");
}

#[test]
fn an_unset_clock_shows_a_placeholder_rather_than_a_fabricated_time() {
    let unset = WallClockReading::new(Time64::UNIX_EPOCH, WallTimeState::Unset);
    assert_eq!(
        spell(unset),
        UNSET_LABEL,
        "00:00 would be a time nobody set"
    );
    let mut clock = SessionClock::new();
    assert!(
        clock.adopt(unset, 0),
        "the placeholder owes the bar a repaint, or the clock is never drawn"
    );
    assert_eq!(clock.label(), UNSET_LABEL);
    assert!(
        !clock.adopt(unset, SEC),
        "still unset a second later is the same label"
    );
    // A firmware-seeded reading *is* set — plausible rather than verified,
    // but a real time — so it draws.
    let seeded = WallClockReading::new(Time64::from_secs(1_709_214_367), WallTimeState::Firmware);
    assert!(clock.adopt(seeded, 0));
    assert_eq!(clock.label(), "13:46");
}

#[test]
fn adopting_reports_only_a_label_that_actually_moved() {
    let mut clock = SessionClock::new();
    assert!(clock.adopt(at(1_709_214_367, 0), 0), "the first reading");
    // Another second in the same minute is the same label: no repaint.
    assert!(!clock.adopt(at(1_709_214_368, 0), SEC));
    // The next minute is a new label.
    assert!(clock.adopt(at(1_709_214_420, 0), 53 * SEC));
    assert_eq!(clock.label(), "13:47");
}

#[test]
fn the_tick_is_armed_at_the_next_minute_boundary_and_never_at_zero() {
    let mut clock = SessionClock::new();
    // 7 seconds into the minute: 53 to go.
    clock.adopt(at(1_709_214_367, 0), 0);
    assert_eq!(clock.park_deadline_ns(0, NO_DEADLINE_NS), 53 * SEC);
    // Sub-second precision is spent too, so the wake lands on the boundary
    // rather than a fraction past it.
    clock.adopt(at(1_709_214_367, 250_000_000), 0);
    assert_eq!(
        clock.park_deadline_ns(0, NO_DEADLINE_NS),
        53 * SEC - 250_000_000
    );
    // Exactly on the boundary waits a whole minute, never zero — a deadline
    // of zero would spin the park.
    clock.adopt(at(1_709_214_360, 0), 0);
    assert_eq!(clock.park_deadline_ns(0, NO_DEADLINE_NS), 60 * SEC);
}

#[test]
fn a_clock_that_has_read_nothing_arms_no_deadline() {
    // An idle desktop must not wake a core for a clock that has never been
    // read: the park is left exactly as it was.
    let clock = SessionClock::new();
    assert_eq!(clock.park_deadline_ns(0, NO_DEADLINE_NS), NO_DEADLINE_NS);
    assert_eq!(clock.park_deadline_ns(12_345, 7 * SEC), 7 * SEC);
}

#[test]
fn a_stale_label_shortens_the_park_to_nothing_because_the_tick_is_owed() {
    let mut clock = SessionClock::new();
    clock.adopt(at(1_709_214_367, 0), 0);
    // The deadline has passed while the loop was busy elsewhere: the tick is
    // still owed, so the next park does not wait for it a second time.
    assert_eq!(clock.park_deadline_ns(53 * SEC, NO_DEADLINE_NS), 0);
    assert_eq!(clock.park_deadline_ns(600 * SEC, NO_DEADLINE_NS), 0);
}

#[test]
fn the_clock_never_lengthens_a_park_another_surface_already_shortened() {
    // The fold only ever tightens: an animation due sooner keeps its own
    // deadline, exactly as `park_within` promises.
    let mut clock = SessionClock::new();
    clock.adopt(at(1_709_214_367, 0), 0);
    assert_eq!(clock.park_deadline_ns(0, SEC / 2), SEC / 2);
}

#[test]
fn a_clock_that_has_read_nothing_is_due() {
    // The first reading is owed unconditionally: there is no label yet, and
    // no deadline that could say when one falls due.
    assert!(SessionClock::new().is_due(0));
    assert!(SessionClock::new().is_due(u64::MAX));
}

/// The gate the run loop reads the wall clock behind, pinned against the
/// deadline it folds into the park.
///
/// Something wakes the desktop every couple of seconds whatever the clock is
/// doing, so `is_due` is what keeps a minute-granular label from costing a
/// syscall on each of those wakes. The two views must agree exactly: a wake
/// the park was *not* shortened for has nothing to ask the clock about, and
/// one it *was* shortened for is owed a reading.
#[test]
fn the_read_is_due_exactly_when_the_park_it_armed_has_elapsed() {
    let mut clock = SessionClock::new();
    // 53 seconds short of the next minute boundary.
    clock.adopt(at(1_709_214_367, 0), 0);

    for now in [0, SEC, 52 * SEC, 53 * SEC - 1] {
        assert!(!clock.is_due(now), "read early at {now}");
        assert!(
            clock.park_deadline_ns(now, NO_DEADLINE_NS) > 0,
            "parked for nothing at {now}"
        );
    }
    for now in [53 * SEC, 53 * SEC + 1, 600 * SEC] {
        assert!(clock.is_due(now), "read owed but not due at {now}");
        assert_eq!(
            clock.park_deadline_ns(now, NO_DEADLINE_NS),
            0,
            "owed a tick but still parking at {now}"
        );
    }
}

#[test]
fn adopting_a_reading_clears_the_debt_until_the_next_minute() {
    let mut clock = SessionClock::new();
    clock.adopt(at(1_709_214_367, 0), 0);
    assert!(clock.is_due(53 * SEC));
    // The tick is paid at the boundary, which arms the following whole minute.
    clock.adopt(at(1_709_214_420, 0), 53 * SEC);
    assert!(!clock.is_due(53 * SEC));
    assert!(!clock.is_due(112 * SEC));
    assert!(clock.is_due(113 * SEC));
}
