//! The desktop-hover vertical's shared contract
//! (`plans/FIX-DESKTOP-SPEEDUP.md` A.4).
//!
//! The freestanding guest gate (`src/main.rs`) and the host runner's enrolment
//! (`tools/xtask/src/commands/qemu_tests.rs`) both read this module, so the
//! gesture the host injects and the bounds the guest asserts cannot drift
//! apart.
//!
//! # What the vertical is for
//!
//! A hover repaints the control the pointer arrived at, and nothing else. That
//! is the whole claim of the per-control damage work, and until now nothing
//! held the running desktop to it: the counters existed and were published,
//! but no test read them back from a real gesture. This is that gate — the one
//! every later stage tightens.
//!
//! # Why a bracketed window, and never a whole-epoch figure
//!
//! The published accounting is cumulative from the session's first frame, and
//! bring-up legitimately composes full-screen frames (the wallpaper, the
//! reveal fade). So the epoch's *mean* damage is bring-up's, and so is its
//! *peak*: neither says anything about the gesture that followed. The gesture
//! is therefore bracketed — one sample before it and one after — and the
//! bounds below apply to the difference, which is load-independent because
//! every counter is work rather than time.
//!
//! # Why the bounds are fractions of the screen
//!
//! A bound written as a pixel count would be a constant that a different
//! display invalidates. Each bound here is derived from the screen extent the
//! epoch was composed against, so the same statement holds on any board.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

use tairix_test_framestats::{Delta, Sample};

/// Bare name of the fixture bundle whose two launches bracket the sweep.
///
/// A command app in the system store, listed in the desktop's program library
/// so a click launches it: the desktop owns the screen, so a click is the only
/// gesture that reaches a program, and a pointer script's steps fire strictly
/// in script order — which is what puts the sweep between the two samples.
pub const SAMPLE_APP_NAME: &str = "framestats";

/// Pointer moves the sweep injects between the two samples.
///
/// Long enough that the frames the sweep composes dominate the bracketed
/// window over the two library popups that open and close inside it, and long
/// enough to span several of the publisher's rate-limited submissions.
pub const SWEEP_MOVES: u32 = 32;

/// Frames the bracketed window must have composed for the run to have
/// measured anything.
///
/// The sweep marks damage whenever the pointer arrives somewhere new, so the
/// frames exist regardless of machine load — only the *publisher's*
/// submissions are rate-limited, and a submission carries the cumulative
/// count rather than one frame. A window that composed fewer frames than this
/// did not carry the gesture, and the run fails rather than passing on an
/// empty difference.
pub const MIN_SWEEP_FRAMES: u64 = 8;

/// Reciprocal of the screen fraction one frame in the bracketed window may
/// recompose, on average.
///
/// An eighth of the screen sits above the measured baseline with room to
/// spare and far below a window (this board's file-manager window is over a
/// third of the screen) or the screen itself, so it catches a gesture that
/// starts repainting either.
///
/// The baseline it is set from is a real one, and it is *not* the cost of a
/// control: the bar escalates every hover change to a whole-bar repaint, which
/// is 25 times the pixels the change can be seen in. That is a known defect
/// recorded in `plans/FIX-DESKTOP-SPEEDUP.md`, not a cost this bound endorses
/// — the bound is where a gate can sit *today* without failing on the desktop
/// as it stands, and the plan states the far tighter one the fix admits.
pub const SWEEP_DAMAGE_DIVISOR: u64 = 8;

/// Reciprocal of the screen fraction the bracketed window may spend
/// recomputing backdrop frosts.
///
/// Not zero, and the difference matters: a frosted surface that newly appears
/// inside the window has to compute its frost once, and the second sample is
/// launched from a frosted popup. What must never happen is a frost recomputed
/// *per frame* — a hover changes no window's backdrop, so every frost already
/// on screen must be served from the retained one. A quarter of the screen
/// admits a handful of one-off frosts and refuses a per-frame recompute by a
/// wide margin.
pub const SWEEP_BLUR_DIVISOR: u64 = 4;

/// Layer contributions one damaged pixel in the bracketed window may cost, on
/// average.
///
/// Blends count contributions rather than positions, so a damaged pixel under
/// a stack of surfaces is blended once per surface; this is the overdraw
/// reading. Four bounds the bar over the wallpaper with the pointer above it
/// and refuses a frame that pays for depth nobody can see.
pub const MAX_BLENDS_PER_DAMAGED_PX: u64 = 4;

/// The screen the bracketed epoch must have been composed against: the
/// board's ramfb console extent.
///
/// Pinned so an epoch taken against some other screen — a stale record, a
/// different session — cannot satisfy a fraction-of-the-screen bound by being
/// tiny.
pub const EXPECTED_SCREEN_PX: u64 =
    tairix_fwcfg::RAMFB_CONSOLE_WIDTH_PX as u64 * tairix_fwcfg::RAMFB_CONSOLE_HEIGHT_PX as u64;

/// Which bound a bracketed window failed, or that it met them all.
///
/// Named rather than boolean so the guest states the failing check in the
/// serial transcript: a run that fails must say what it measured and which
/// rule that broke.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Verdict {
    /// Every bound held.
    Held,
    /// The two samples do not describe one continuous epoch — a counter went
    /// backwards, or the screen extent changed between them.
    EpochBroken,
    /// The epoch was composed against some other screen.
    ScreenExtent,
    /// The window composed too few frames to have carried the gesture.
    TooFewFrames,
    /// The average frame recomposed more than its share of the screen.
    DamagePerFrame,
    /// The average damaged pixel cost more layer contributions than the
    /// bound.
    Overdraw,
    /// More frost work than a handful of newly-shown surfaces can account
    /// for — the signature of a frost recomputed per frame rather than served
    /// from the retained one.
    FrostWork,
    /// Window furniture was re-rendered. A hover mutates no window, so every
    /// furniture lookup must have been a cache hit.
    ChromeRerendered,
    /// More driver calls than one per dirty rectangle plus one per frame.
    PresentsPerFrame,
}

impl Verdict {
    /// Whether the window met every bound.
    #[must_use]
    pub const fn held(self) -> bool {
        matches!(self, Self::Held)
    }

    /// A short, stable name for the transcript.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Held => "held",
            Self::EpochBroken => "epoch-broken",
            Self::ScreenExtent => "screen-extent",
            Self::TooFewFrames => "too-few-frames",
            Self::DamagePerFrame => "damage-per-frame",
            Self::Overdraw => "overdraw",
            Self::FrostWork => "frost-work",
            Self::ChromeRerendered => "chrome-rerendered",
            Self::PresentsPerFrame => "presents-per-frame",
        }
    }
}

/// Judge the window bracketed by `before` and `after`.
///
/// The checks are ordered so the first thing reported is the most fundamental:
/// a broken epoch or a foreign screen means the pair measured nothing, and
/// there is no point reporting a damage figure taken across it.
#[must_use]
pub fn assess(before: &Sample, after: &Sample) -> Verdict {
    let Some(delta) = before.work_until(after) else {
        return Verdict::EpochBroken;
    };
    if delta.screen_px != EXPECTED_SCREEN_PX {
        return Verdict::ScreenExtent;
    }
    judge(&delta)
}

/// Judge an already-differenced window.
#[must_use]
pub fn judge(delta: &Delta) -> Verdict {
    let Some(per_frame) = delta.damage_per_frame() else {
        return Verdict::TooFewFrames;
    };
    if delta.frames < MIN_SWEEP_FRAMES {
        return Verdict::TooFewFrames;
    }
    if per_frame > delta.screen_px / SWEEP_DAMAGE_DIVISOR {
        return Verdict::DamagePerFrame;
    }
    if delta.blended_px > delta.damaged_px.saturating_mul(MAX_BLENDS_PER_DAMAGED_PX) {
        return Verdict::Overdraw;
    }
    if delta.blur_px > delta.screen_px / SWEEP_BLUR_DIVISOR {
        return Verdict::FrostWork;
    }
    if delta.chrome_misses != 0 {
        return Verdict::ChromeRerendered;
    }
    if delta.present_calls > delta.dirty_rects.saturating_add(delta.frames) {
        return Verdict::PresentsPerFrame;
    }
    Verdict::Held
}

#[cfg(test)]
mod tests {
    use super::{
        assess, judge, Verdict, EXPECTED_SCREEN_PX, MAX_BLENDS_PER_DAMAGED_PX, MIN_SWEEP_FRAMES,
        SWEEP_BLUR_DIVISOR, SWEEP_DAMAGE_DIVISOR, SWEEP_MOVES,
    };
    use tairix_test_framestats::{Delta, Sample};

    /// A window a per-control hover would plausibly produce: every sweep move
    /// repainting one bar control and the pointer's own rectangles.
    fn hovering() -> Delta {
        Delta {
            screen_px: EXPECTED_SCREEN_PX,
            frames: 40,
            damaged_px: 40 * 4_000,
            blended_px: 40 * 6_000,
            // One newly-shown frosted surface, which is what the second
            // sample's own launch popup costs.
            blur_px: 40_560,
            dirty_rects: 84,
            present_calls: 40,
            chrome_misses: 0,
        }
    }

    #[test]
    fn a_per_control_hover_holds_every_bound() {
        assert_eq!(judge(&hovering()), Verdict::Held);
        assert!(judge(&hovering()).held());
    }

    #[test]
    fn a_sweep_that_repaints_the_screen_fails_the_damage_bound() {
        let mut screenful = hovering();
        screenful.damaged_px = screenful.frames * EXPECTED_SCREEN_PX;
        assert_eq!(judge(&screenful), Verdict::DamagePerFrame);
    }

    #[test]
    fn the_damage_bound_is_a_share_of_the_screen_and_binds_at_it() {
        let share = EXPECTED_SCREEN_PX / SWEEP_DAMAGE_DIVISOR;
        let mut at_bound = hovering();
        at_bound.damaged_px = at_bound.frames * share;
        assert_eq!(judge(&at_bound), Verdict::Held, "the bound itself passes");

        let mut over = at_bound;
        over.damaged_px += 1;
        assert_eq!(
            judge(&over),
            Verdict::DamagePerFrame,
            "one pixel past the bound fails, because the mean rounds up"
        );
    }

    #[test]
    fn an_empty_window_never_passes() {
        let mut empty = hovering();
        empty.frames = 0;
        empty.damaged_px = 0;
        assert_eq!(judge(&empty), Verdict::TooFewFrames);
    }

    #[test]
    fn a_window_short_of_the_gesture_never_passes() {
        let mut short = hovering();
        short.frames = MIN_SWEEP_FRAMES - 1;
        short.damaged_px = short.frames * 4_000;
        assert_eq!(judge(&short), Verdict::TooFewFrames);
    }

    #[test]
    fn overdraw_is_bounded_per_damaged_pixel() {
        let mut deep = hovering();
        deep.blended_px = deep.damaged_px * MAX_BLENDS_PER_DAMAGED_PX;
        assert_eq!(judge(&deep), Verdict::Held, "the bound itself passes");
        deep.blended_px += 1;
        assert_eq!(judge(&deep), Verdict::Overdraw);
    }

    #[test]
    fn one_off_frost_work_passes_and_a_frost_per_frame_does_not() {
        let share = EXPECTED_SCREEN_PX / SWEEP_BLUR_DIVISOR;
        let mut at_bound = hovering();
        at_bound.blur_px = share;
        assert_eq!(judge(&at_bound), Verdict::Held, "the bound itself passes");

        let mut per_frame = hovering();
        // A bar-sized frost recomputed on every frame of the sweep.
        per_frame.blur_px = per_frame.frames * 40_560;
        assert_eq!(judge(&per_frame), Verdict::FrostWork);
    }

    #[test]
    fn re_rendered_furniture_fails_the_run() {
        let mut missed = hovering();
        missed.chrome_misses = 1;
        assert_eq!(judge(&missed), Verdict::ChromeRerendered);
    }

    #[test]
    fn more_driver_calls_than_rectangles_and_frames_fails_the_run() {
        let mut chatty = hovering();
        chatty.present_calls = chatty.dirty_rects + chatty.frames + 1;
        assert_eq!(judge(&chatty), Verdict::PresentsPerFrame);
    }

    #[test]
    fn a_broken_epoch_is_reported_before_any_figure_taken_across_it() {
        let before = Sample {
            screen_px: EXPECTED_SCREEN_PX,
            frames: 100,
            damaged_px: 10_000_000,
            ..Sample::default()
        };
        let after = Sample {
            frames: 3,
            damaged_px: 9_000,
            ..before
        };
        assert_eq!(assess(&before, &after), Verdict::EpochBroken);
    }

    #[test]
    fn an_epoch_against_another_screen_is_refused() {
        let before = Sample {
            screen_px: 64,
            frames: 1,
            ..Sample::default()
        };
        let after = Sample {
            frames: 200,
            damaged_px: 1_000,
            dirty_rects: 200,
            present_calls: 200,
            ..before
        };
        assert_eq!(assess(&before, &after), Verdict::ScreenExtent);
    }

    #[test]
    fn a_bracketed_per_control_hover_passes_through_the_sample_pair() {
        let before = Sample {
            screen_px: EXPECTED_SCREEN_PX,
            frames: 30,
            damaged_px: 20_000_000,
            blended_px: 24_000_000,
            blur_px: 90_000,
            dirty_rects: 40,
            present_calls: 30,
            chrome_misses: 4,
        };
        let hover = hovering();
        let after = Sample {
            screen_px: before.screen_px,
            frames: before.frames + hover.frames,
            damaged_px: before.damaged_px + hover.damaged_px,
            blended_px: before.blended_px + hover.blended_px,
            blur_px: before.blur_px + hover.blur_px,
            dirty_rects: before.dirty_rects + hover.dirty_rects,
            present_calls: before.present_calls + hover.present_calls,
            chrome_misses: before.chrome_misses + hover.chrome_misses,
        };
        assert_eq!(
            assess(&before, &after),
            Verdict::Held,
            "bring-up's own full-screen frames are outside the window"
        );
    }

    #[test]
    fn the_sweep_is_long_enough_to_clear_the_frame_floor() {
        assert!(
            u64::from(SWEEP_MOVES) > MIN_SWEEP_FRAMES,
            "the injected gesture must be able to compose the frames the gate demands"
        );
    }

    #[test]
    fn every_verdict_names_itself() {
        for verdict in [
            Verdict::Held,
            Verdict::EpochBroken,
            Verdict::ScreenExtent,
            Verdict::TooFewFrames,
            Verdict::DamagePerFrame,
            Verdict::Overdraw,
            Verdict::FrostWork,
            Verdict::ChromeRerendered,
            Verdict::PresentsPerFrame,
        ] {
            assert!(!verdict.as_str().is_empty());
            assert_eq!(verdict.held(), verdict == Verdict::Held);
        }
    }
}
