//! The taskbar clock's wall-time reading and its once-a-minute tick.
//!
//! The bar itself holds only the *string* it draws
//! ([`tairix_taskbar::Clock`]) and carries no time ABI. This is the session
//! side: it reads the wall clock, spells the reading `HH:mm`, and says when
//! the label next goes stale.
//!
//! # UTC, because there is no other truth here
//!
//! TAIRiX keeps no timezone offset, so the reading is UTC and the shared
//! [`CivilTime`] breakdown is what turns an absolute instant into calendar
//! fields — the same one `ls`'s date column and the login clock read, never a
//! second copy of the day/minute arithmetic. Only the *spelling* is this
//! module's: `HH:mm` is the bar's own, and `CivilTime`'s rustdoc leaves
//! presentation to each consumer.
//!
//! # An unset clock says so, and is still a clock
//!
//! A machine whose wall time has never been established this boot reports
//! [`WallTimeState::Unset`](tairix_abi::time::WallTimeState::Unset) — the
//! Unix-epoch placeholder, which carries no real-world meaning. The label is
//! then the bar's own [`UNSET_LABEL`]: the shape of a time with no time in
//! it. `00:00` would be a fabricated reading, and an empty label would leave
//! the bar's clock — whose menu is where a time is
//! set — invisible at exactly the moment the user most needs to reach it.
//!
//! # One wake a minute, and none when nothing can be seen
//!
//! A clock is the one thing on the desktop that must change without anybody
//! touching anything, so it is the one thing that arms a deadline of its own.
//! It arms exactly one: the next minute boundary, folded into the session's
//! park through the same shared [`park_within`] every animated surface uses.
//! That is the fewest wakes a minute-granular clock can be right with — not a
//! poll, and nothing like the busy-wait the desktop forbids elsewhere. A
//! background session folds no deadlines at all, so its clock ticks not at
//! all: there is nothing on screen for it to be wrong on.
//!
//! The same deadline is also the *read* gate ([`SessionClock::is_due`]).
//! Plenty else wakes the session — a tray reading arrives every couple of
//! seconds — and reading the wall clock on each of those would put a syscall
//! on a path with nothing to ask about. The clock is read on the wakes it
//! shortened the park for, and on no others.

use alloc::string::String;
use core::fmt::Write as _;

use tairix_abi::time::CivilTime;
use tairix_abi::time::{Time64, WallClockReading};
use tairix_taskbar::clock::UNSET_LABEL;

use crate::switchuser::park_within;

/// Nanoseconds in one second.
const NANOS_PER_SEC: u64 = 1_000_000_000;

/// Seconds in one minute.
const SECS_PER_MIN: i64 = 60;

/// The session's clock: the label it last pushed to the bar, and when that
/// label goes stale.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SessionClock {
    /// The label last handed to the bar. Empty only before the first reading
    /// has been spelled; a reading that means nothing spells the bar's
    /// [`UNSET_LABEL`].
    label: String,
    /// The monotonic instant the current label stops being right, or `None`
    /// before the first reading — there is then nothing to go stale.
    stale_at_ns: Option<u64>,
}

impl SessionClock {
    /// A clock that has read nothing yet.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            label: String::new(),
            stale_at_ns: None,
        }
    }

    /// The label the bar should draw, empty only before the first reading.
    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    /// Whether the wall clock is worth reading again: nothing has been read
    /// yet, or the minute the current label was right for has turned.
    ///
    /// The same deadline [`park_deadline_ns`](Self::park_deadline_ns) folds
    /// into the park, asked the other way round — so the loop reads the clock
    /// exactly on the wakes it shortened the park *for*, and not on the many
    /// it did not.
    #[must_use]
    pub fn is_due(&self, now_ns: u64) -> bool {
        self.stale_at_ns.is_none_or(|stale| now_ns >= stale)
    }

    /// Adopt `reading` as of monotonic instant `now_ns`, answering whether the
    /// label changed and so owes the bar a repaint.
    ///
    /// The next minute boundary is computed from the reading's own
    /// second-of-minute and measured against the *monotonic* clock, because
    /// that is what the park is armed on: a wall clock that is stepped while
    /// the desktop waits moves the label at the next tick rather than moving
    /// the deadline under it.
    pub fn adopt(&mut self, reading: WallClockReading, now_ns: u64) -> bool {
        let next = spell(reading);
        // Even an unset reading arms the tick: the wall clock may be set
        // while the desktop is up, and the clock that noticed would otherwise
        // be the one that never wakes.
        self.stale_at_ns = Some(now_ns.saturating_add(until_next_minute_ns(reading.time())));
        if next == self.label {
            return false;
        }
        self.label = next;
        true
    }

    /// `park_ns` shortened to the moment this label goes stale, or left
    /// exactly as it is before the first reading.
    ///
    /// A label already stale shortens the park to nothing rather than leaving
    /// it alone: the tick is owed, and only [`adopt`](Self::adopt) pays it.
    #[must_use]
    pub fn park_deadline_ns(&self, now_ns: u64, park_ns: u64) -> u64 {
        park_within(
            park_ns,
            self.stale_at_ns
                .map(|stale| stale.saturating_sub(now_ns).min(park_ns)),
        )
    }
}

/// The `HH:mm` spelling of `reading`, or [`UNSET_LABEL`] when it states no
/// real time.
///
/// Public because aiming at the clock is the same fact as reading it back: a
/// host-side observer of the desktop — the QEMU vertical's screendumps — finds
/// the label by spelling the instant it staged, rather than restating the
/// format.
#[must_use]
pub fn spell(reading: WallClockReading) -> String {
    if !reading.state().is_set() {
        return String::from(UNSET_LABEL);
    }
    let civil = CivilTime::from_time64(reading.time());
    let mut out = String::new();
    // Writing into a `String` never fails; the `Result` is discarded
    // deliberately rather than unwrapped.
    let _ = write!(out, "{:02}:{:02}", civil.hour, civil.minute);
    out
}

/// Nanoseconds from `time` to the next whole minute — never zero, so a
/// reading taken exactly on the boundary waits a whole minute rather than
/// arming a deadline that has already passed.
fn until_next_minute_ns(time: Time64) -> u64 {
    // Euclidean remainder, so an instant before the epoch counts forward to
    // the next boundary exactly as one after it does.
    let into_minute = time.secs().rem_euclid(SECS_PER_MIN);
    // `into_minute` is `0..60`, so the difference is `1..=60`.
    let secs = u64::try_from(SECS_PER_MIN - into_minute).unwrap_or(u64::from(1u8));
    secs.saturating_mul(NANOS_PER_SEC)
        .saturating_sub(u64::from(time.subsec_nanos()))
}

#[cfg(test)]
#[path = "clock_tests.rs"]
mod tests;
