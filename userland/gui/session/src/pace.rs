//! The session's frame deadline: at most one composite per frame period,
//! however many wakes produced damage inside it.
//!
//! Without it the desktop composites once per *wake*. A hand on the mouse
//! delivers pointer samples far faster than any screen can show them, so a
//! drag spends whole frames' worth of blending on pixels the next sample
//! overwrites before a scan-out ever reads them. Damage accumulates in the
//! compositor between deadlines instead, and is composited once when the
//! next one arrives.
//!
//! # Latency is paid only where a frame would have been wasted
//!
//! A frame whose period has already elapsed — the first after an idle
//! desktop, a click, a keystroke, any interaction slower than the display —
//! is admitted on the very wake that produced it. Only a producer outrunning
//! the screen is held, and only until the frame it is racing.
//!
//! A held frame shortens the session's park to the moment it comes due,
//! through the same [`park_within`] fold the clock and every animated surface
//! use. Nothing held means nothing armed: an idle desktop parks indefinitely
//! and the pacer costs it not one wake.
//!
//! # The period is the one the desktop already animates at
//!
//! [`Timeline::FRAME_NS`] is the shortest gap between two frames worth
//! drawing, which is the same fact whether a frame carries an animation step
//! or a drag — so there is no second frame-period constant, and an animated
//! surface is never woken for a frame the pacer would then refuse. Real vsync
//! takes the deadline from the flip a display driver signals, in this same
//! place (`plans/FIX-DISPLAY-ACCELERATION.md`); no driver reports a refresh
//! today, so a mode field for one would be an ABI with no producer.

use tairix_theme::Timeline;

use crate::switchuser::park_within;

/// The session's frame pacer: when the last composite was admitted, and
/// whether one is being held for the next deadline.
///
/// One per session, driven from the run loop's frame path. It holds no copy
/// of the damage — the compositor owns that, and answers whether a composite
/// would recompose a pixel — so the two cannot disagree about whether a
/// frame is owed.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct FramePacer {
    /// When the last *composited* frame was admitted. `None` until the first,
    /// so a desktop coming up draws at once.
    last_ns: Option<u64>,
    /// Whether damage is being held for the next deadline. Drives
    /// [`park_deadline_ns`](Self::park_deadline_ns).
    held: bool,
}

impl FramePacer {
    /// A session that has composited nothing yet.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            last_ns: None,
            held: false,
        }
    }

    /// Whether the frame this wake produced is composited and presented now,
    /// rather than held for the next deadline.
    ///
    /// `damaged` is the compositor's own answer to whether a composite would
    /// recompose a pixel. An undamaged frame is never held and never starts
    /// the period: presenting it moves nothing, and it is what re-reads the
    /// frame counters as idle, so holding it would suppress that reading and
    /// starting the period would delay the next real frame behind it.
    ///
    /// A clock that has jumped behind the last frame admits rather than
    /// stalling the screen for the length of the jump.
    pub fn admit(&mut self, now_ns: u64, damaged: bool) -> bool {
        if damaged && self.remaining_ns(now_ns) != 0 {
            self.held = true;
            return false;
        }
        self.held = false;
        if damaged {
            self.last_ns = Some(now_ns);
        }
        true
    }

    /// `park_ns` shortened to the moment a held frame comes due, or left
    /// exactly as it is when nothing is held.
    ///
    /// This is the whole of the pacer's wake behaviour: one shot per held
    /// frame, and none at all otherwise. The deadline is never shortened to
    /// nothing while a frame is genuinely being held —
    /// [`admit`](Self::admit) holds only what is not yet due — so the loop
    /// cannot spin between a refusal and its deadline.
    #[must_use]
    pub fn park_deadline_ns(&self, now_ns: u64, park_ns: u64) -> u64 {
        park_within(park_ns, self.held.then(|| self.remaining_ns(now_ns)))
    }

    /// Nanoseconds left of the current frame period: `0` once the next frame
    /// is due, before the first frame, and on a clock that jumped backwards.
    fn remaining_ns(&self, now_ns: u64) -> u64 {
        self.last_ns.map_or(0, |last| {
            now_ns
                .checked_sub(last)
                .map_or(0, |elapsed| Timeline::FRAME_NS.saturating_sub(elapsed))
        })
    }
}

#[cfg(test)]
#[path = "pace_tests.rs"]
mod tests;
