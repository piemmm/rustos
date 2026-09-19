//! The simulation's clock, and how a frame reads between its ticks.
//!
//! The simulation steps at a fixed rate and the display refreshes at
//! whatever rate it has. Tying one to the other makes the sim rate a
//! visual property — motion that stutters on a 60 Hz panel and runs fast
//! on a 144 Hz one — so they are separate: the pacer converts elapsed
//! real time into whole ticks, and what is left over is the fraction a
//! frame reads *between* the last two authoritative states.
//!
//! # No drift, and no death spiral
//!
//! The accumulator counts in nanosecond-ticks rather than in nanoseconds
//! divided by a tick, so a rate that does not divide a second evenly —
//! thirty of them do not — loses nothing over an hour.
//!
//! A frame that took far longer than a tick cannot be paid back in full
//! without taking even longer, so the catch-up is bounded: time beyond
//! [`MAX_CATCHUP_NS`] is dropped rather than replayed. A client that was
//! descheduled for a second resumes rather than spending the next second
//! simulating the last one.

use tairix_wintersun_net::value::WorldPoint;
use tairix_wintersun_rules::clock::TickRate;

/// Nanoseconds in a second.
const NS_PER_SEC: u64 = 1_000_000_000;

/// The most elapsed time one advance will replay.
///
/// A quarter second: long enough to absorb a scheduling hiccup or a slow
/// frame, short enough that a client returning from a long stall does not
/// then spend longer catching up than it was away.
pub const MAX_CATCHUP_NS: u64 = NS_PER_SEC / 4;

/// How many whole ticks have elapsed, and how far into the next one the
/// display is.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Pacer {
    rate: TickRate,
    /// Elapsed time not yet spent, in nanosecond-ticks.
    accumulator: u64,
    last: Option<u64>,
    paused: bool,
}

impl Pacer {
    /// A pacer for `rate`, not yet started.
    #[must_use]
    pub const fn new(rate: TickRate) -> Self {
        Self {
            rate,
            accumulator: 0,
            last: None,
            paused: false,
        }
    }

    /// The tick rate.
    #[must_use]
    pub const fn rate(&self) -> TickRate {
        self.rate
    }

    /// Whether the clock is stopped.
    #[must_use]
    pub const fn paused(&self) -> bool {
        self.paused
    }

    /// How far between the last two authoritative states a frame should
    /// read, out of 255.
    #[must_use]
    pub fn alpha(&self) -> u8 {
        u8::try_from(self.accumulator.saturating_mul(255) / NS_PER_SEC).unwrap_or(u8::MAX)
    }

    /// How many whole ticks to step for a clock now reading `now_ns`.
    ///
    /// The first call after a start or a resume establishes the reading
    /// and steps nothing: the gap before the clock was being watched is
    /// not elapsed simulation time.
    pub fn advance(&mut self, now_ns: u64) -> u32 {
        let Some(last) = self.last.replace(now_ns) else {
            return 0;
        };
        if self.paused {
            return 0;
        }
        let elapsed = now_ns.saturating_sub(last).min(MAX_CATCHUP_NS);
        self.accumulator = self
            .accumulator
            .saturating_add(elapsed.saturating_mul(u64::from(self.rate.hz())));
        let ticks = self.accumulator / NS_PER_SEC;
        self.accumulator %= NS_PER_SEC;
        u32::try_from(ticks).unwrap_or(u32::MAX)
    }

    /// Stop the clock, keeping the fraction the display is showing.
    ///
    /// A seat taken away by a fast user switch is not simulated time, and
    /// the frame on screen when it went is the frame that comes back.
    pub fn pause(&mut self) {
        self.paused = true;
        self.last = None;
    }

    /// Start the clock again from `now_ns`, exactly where it stopped.
    pub fn resume(&mut self, now_ns: u64) {
        self.paused = false;
        self.last = Some(now_ns);
    }
}

/// A position as it was at the last two ticks.
///
/// What the camera follows, and the shape every moving thing a later item
/// draws reads between: a frame samples the interpolation, never the
/// authoritative state directly, so motion is smooth at any display rate.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub struct Motion {
    previous: WorldPoint,
    current: WorldPoint,
}

impl Motion {
    /// A motion standing still at `at`.
    #[must_use]
    pub const fn still(at: WorldPoint) -> Self {
        Self {
            previous: at,
            current: at,
        }
    }

    /// Record the authoritative position after a tick.
    pub fn observe(&mut self, at: WorldPoint) {
        self.previous = self.current;
        self.current = at;
    }

    /// Forget the previous position, so the next frame reads `at` exactly.
    ///
    /// For a jump the display must not interpolate across — a teleport, a
    /// zone handover, a resume into a different place — where a blend
    /// would draw the body streaking over ground it never crossed.
    pub fn snap(&mut self, at: WorldPoint) {
        self.previous = at;
        self.current = at;
    }

    /// The authoritative position.
    #[must_use]
    pub const fn current(&self) -> WorldPoint {
        self.current
    }

    /// Where to draw it at `alpha` between the last two ticks.
    #[must_use]
    pub fn at(&self, alpha: u8) -> WorldPoint {
        interpolate(self.previous, self.current, alpha)
    }
}

/// A point `alpha`/255 of the way from `a` to `b`.
#[must_use]
pub fn interpolate(a: WorldPoint, b: WorldPoint, alpha: u8) -> WorldPoint {
    let axis = |from: i32, to: i32| {
        let moved = i64::from(from) + (i64::from(to) - i64::from(from)) * i64::from(alpha) / 255;
        i32::try_from(moved).unwrap_or(from)
    };
    WorldPoint {
        x: axis(a.x, b.x),
        y: axis(a.y, b.y),
    }
}

#[cfg(test)]
#[path = "pacing_tests.rs"]
mod tests;
