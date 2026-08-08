//! When the login screen next has something to do.
//!
//! An idle login screen must consume no CPU, so the event loop parks on the
//! seat's input and wakes on a deadline rather than polling. There are only
//! two things that repaint without an input event — the clock reaching the
//! next minute, and a lockout counting down — so the deadline is the nearer
//! of those, and there is no deadline at all when neither applies.

use tairix_abi::time::{Duration64, Time64};

/// Nanoseconds in one second.
const NANOS_PER_SEC: u64 = 1_000_000_000;

/// Seconds in one minute.
const SECS_PER_MINUTE: i64 = 60;

/// A relative timeout meaning "wait until something arrives".
pub const FOREVER: u64 = u64::MAX;

/// The authority's per-account lockout, counted down against the monotonic
/// clock.
///
/// The surface presents a remaining span and reads no clock of its own, so
/// the countdown lives here. It is monotonic-clock-driven, so a wall-clock
/// correction cannot shorten or lengthen a lockout.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct Cooldown {
    until: Option<u64>,
}

impl Cooldown {
    /// Begin — or replace — a lockout of `retry_after` starting now.
    ///
    /// A zero or negative span is not a lockout and clears any standing one,
    /// so an accepted secret or an unanswerable one leaves nothing behind.
    pub fn start(&mut self, now_ns: u64, retry_after: Duration64) {
        let span = retry_after.saturating_total_nanos();
        self.until = (span > 0).then(|| now_ns.saturating_add(span));
    }

    /// How much of the lockout is left, zero once it has run out.
    #[must_use]
    pub fn remaining(&self, now_ns: u64) -> Duration64 {
        match self.until {
            Some(until) => Duration64::from_nanos(until.saturating_sub(now_ns)),
            None => Duration64::ZERO,
        }
    }

    /// Whether a lockout is still standing.
    #[must_use]
    pub fn is_running(&self, now_ns: u64) -> bool {
        self.until.is_some_and(|until| until > now_ns)
    }
}

/// The relative nanosecond timeout for the next park.
///
/// [`FOREVER`] means "no deadline": nothing on screen changes until an input
/// event arrives, which is the resting state of an untouched login screen.
/// `now` is `None` when no trusted wall time is held, in which case there is
/// no clock on the backdrop to keep current either.
#[must_use]
pub fn park_timeout(now: Option<Time64>, cooldown_remaining: Duration64) -> u64 {
    let clock = now.map(nanos_to_next_minute);
    let remaining = cooldown_remaining.saturating_total_nanos();
    let tick = (remaining > 0).then(|| remaining.min(NANOS_PER_SEC));
    match (clock, tick) {
        (Some(clock), Some(tick)) => clock.min(tick),
        (Some(only), None) | (None, Some(only)) => only,
        (None, None) => FOREVER,
    }
}

/// Nanoseconds from `now` to the next whole minute.
///
/// Never zero: a reading exactly on a minute boundary has just been drawn,
/// so its next repaint is a whole minute away and a zero timeout would spin.
fn nanos_to_next_minute(now: Time64) -> u64 {
    let into_minute = now.secs().rem_euclid(SECS_PER_MINUTE);
    let secs_left = (SECS_PER_MINUTE - into_minute).unsigned_abs();
    secs_left
        .saturating_mul(NANOS_PER_SEC)
        .saturating_sub(u64::from(now.subsec_nanos()))
}

#[cfg(test)]
#[path = "wait_tests.rs"]
mod tests;
