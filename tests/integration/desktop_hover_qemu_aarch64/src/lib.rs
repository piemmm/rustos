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

/// The most screens' worth of pixels the **whole** bracketed window may
/// recompose.
///
/// A total rather than a per-frame mean, and that is the point. The window's
/// cost is overwhelmingly *fixed* work — the two launcher popups opening,
/// showing a row and closing — with the sweep's own per-control repaints a
/// few per cent of it. Divided by a frame count the host chooses, that fixed
/// cost reads as a small mean on a machine that composed many frames and a
/// large one on a machine that coalesced them, so the bound was met or
/// missed by how busy the host was rather than by what the desktop did. It
/// is the same defect [`MAX_BLUR_PX_PER_DAMAGED_PX`] was reshaped away from,
/// and a total has no denominator to be gamed by: holding frames can only
/// *coalesce* damage, never multiply it, so a loaded host measures less.
///
/// Three screens, against a measured 0.66–1.00 over five runs of the settled
/// gesture (520 713 – 788 713 px of a 786 432-px screen). One further whole
/// screen is allowed on top of that, because a relist, an arriving ground,
/// or a settings change legitimately repaints the desktop layer whole and
/// can fall inside the window on a slow boot — so an honest run reaches at
/// most about two. It refuses by a screen the regression it was re-derived
/// against: a desktop that re-damaged its whole icon bar after every
/// published frame spent 3.69–4.72 screens on the same gesture.
///
/// **It is deliberately not tightened onto the per-control cost, because
/// this window does not measure one.** The launcher churn dominates it, so a
/// tighter figure here would be a bound on the launch path with the hover's
/// own cost lost inside it. The per-control claim is gated where it is
/// deterministic: the host-side sweep in `userland/gui/session` asserts that
/// no frame of the gesture recomposes a whole bar's worth of pixels and that
/// the mean stays under an eighth of one
/// (`plans/FIX-DESKTOP-SPEEDUP.md` C.7).
pub const MAX_SWEEP_SCREENS: u64 = 3;

/// Recomputed frost pixels one damaged pixel in the bracketed window may
/// cost.
///
/// A hover changes no window's backdrop, so every frost already on screen must
/// be served from the retained one; what a frame may legitimately re-frost is
/// what it *damaged* — a newly-shown frosted surface (the second sample's own
/// launch popup) is damaged over its whole area and frosted over it, and a
/// pointer crossing a frosted surface invalidates the strip it uncovers. So
/// one recomputed frost pixel per damaged pixel is the honest ceiling, and a
/// frost recomputed while nothing beneath it changed breaks it by the whole
/// area of the surface.
///
/// Bounded against the damage rather than against the screen because the
/// bracketed epoch's length is not fixed: an absolute share of one screen is
/// met or missed by how many frames the host let the desktop compose, so it
/// failed a sweep that recomputed no frost per frame and held one that did
/// (measured: an identical ~2000 recomputed frost px per frame passed at 61
/// frames and failed at 119). Every other bound here is normalised against
/// what varies; this one now is too.
pub const MAX_BLUR_PX_PER_DAMAGED_PX: u64 = 1;

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
    /// The window recomposed more screens' worth of pixels than the bound.
    DamageTotal,
    /// The average damaged pixel cost more layer contributions than the
    /// bound.
    Overdraw,
    /// More frost recomputed than the frame damaged — the signature of a
    /// frost rebuilt while nothing beneath it changed, rather than served
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
            Self::DamageTotal => "damage-total",
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
    if delta.frames < MIN_SWEEP_FRAMES {
        return Verdict::TooFewFrames;
    }
    if delta.damaged_px > delta.screen_px.saturating_mul(MAX_SWEEP_SCREENS) {
        return Verdict::DamageTotal;
    }
    if delta.blended_px > delta.damaged_px.saturating_mul(MAX_BLENDS_PER_DAMAGED_PX) {
        return Verdict::Overdraw;
    }
    if delta.blur_px > delta.damaged_px.saturating_mul(MAX_BLUR_PX_PER_DAMAGED_PX) {
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
        assess, judge, Verdict, EXPECTED_SCREEN_PX, MAX_BLENDS_PER_DAMAGED_PX,
        MAX_BLUR_PX_PER_DAMAGED_PX, MAX_SWEEP_SCREENS, MIN_SWEEP_FRAMES, SWEEP_MOVES,
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
        assert_eq!(judge(&screenful), Verdict::DamageTotal);
    }

    #[test]
    fn the_damage_bound_is_a_count_of_screens_and_binds_at_it() {
        let mut at_bound = hovering();
        at_bound.damaged_px = EXPECTED_SCREEN_PX * MAX_SWEEP_SCREENS;
        assert_eq!(judge(&at_bound), Verdict::Held, "the bound itself passes");

        let mut over = at_bound;
        over.damaged_px += 1;
        assert_eq!(
            judge(&over),
            Verdict::DamageTotal,
            "one pixel past it fails"
        );
    }

    /// The regression the bound was re-derived against: the desktop
    /// re-damaged its whole icon bar after every published frame, so a
    /// gesture that should cost one control a sample cost a full-width strip
    /// instead.
    ///
    /// Measured on the running guest either side of the fix — 2 901 320 to
    /// 3 711 067 px before, 520 713 to 788 713 after — and the figures are
    /// entered here rather than described, so the bound cannot be loosened
    /// past the defect, or tightened onto the honest cost, without this
    /// failing.
    #[test]
    fn a_bar_re_damaged_every_frame_fails_however_many_frames_it_took() {
        for (damaged, frames) in [(2_901_320u64, 39u64), (3_711_067, 39), (2_937_975, 36)] {
            let mut regressed = hovering();
            regressed.frames = frames;
            regressed.damaged_px = damaged;
            assert_eq!(
                judge(&regressed),
                Verdict::DamageTotal,
                "{damaged} px over {frames} frames must fail"
            );
        }
        for (damaged, frames) in [(520_713u64, 38u64), (586_950, 37), (788_713, 40)] {
            let mut fixed = hovering();
            fixed.frames = frames;
            fixed.damaged_px = damaged;
            assert_eq!(
                judge(&fixed),
                Verdict::Held,
                "the measured cost after the fix must pass"
            );
        }
    }

    /// The load dependence the total replaces: the same work judged the same
    /// however many frames the host let the desktop compose.
    ///
    /// A mean over the frame count read a fixed cost as small on a fast host
    /// and large on one that coalesced, so the gate failed a desktop that had
    /// done nothing wrong. Holding frames only merges damage, so the total
    /// cannot rise with a busy host.
    #[test]
    fn the_damage_bound_does_not_move_with_the_windows_length() {
        for frames in [MIN_SWEEP_FRAMES, 16, 37, 61, 119] {
            let mut epoch = hovering();
            epoch.frames = frames;
            epoch.damaged_px = 788_713;
            assert_eq!(
                judge(&epoch),
                Verdict::Held,
                "the measured honest cost must hold at {frames} frames"
            );

            let mut regressed = epoch;
            regressed.damaged_px = 3_711_067;
            assert_eq!(
                judge(&regressed),
                Verdict::DamageTotal,
                "and the regression must fail at {frames} frames"
            );
        }
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
    fn frost_work_is_bounded_by_the_damage_it_serves() {
        let mut at_bound = hovering();
        at_bound.blur_px = at_bound.damaged_px * MAX_BLUR_PX_PER_DAMAGED_PX;
        assert_eq!(judge(&at_bound), Verdict::Held, "the bound itself passes");
        at_bound.blur_px += 1;
        assert_eq!(judge(&at_bound), Verdict::FrostWork);

        let mut per_frame = hovering();
        // A bar-sized frost recomputed on every frame of the sweep, while the
        // sweep damaged only what the pointer moved over.
        per_frame.blur_px = per_frame.frames * 40_560;
        assert_eq!(judge(&per_frame), Verdict::FrostWork);
    }

    /// The bound holds whatever the host let the desktop compose: the same
    /// per-frame frost cost must judge the same at any epoch length.
    ///
    /// This is the flake it replaces — an absolute share of one screen was met
    /// at 61 frames and missed at 119 with the recomputed frost per frame
    /// unchanged, so a busy host failed a desktop that had done nothing wrong.
    #[test]
    fn the_frost_bound_does_not_move_with_the_epochs_length() {
        for frames in [40u64, 61, 119, 296, 590] {
            let mut epoch = hovering();
            epoch.frames = frames;
            // The ratio the failing and passing runs both measured, over a
            // total the gesture can actually produce: what varies here is
            // how many frames the host let that same work be spread over.
            epoch.damaged_px = 788_713;
            epoch.blur_px = epoch.damaged_px / 6;
            assert_eq!(
                judge(&epoch),
                Verdict::Held,
                "a damage-proportional frost must hold at {frames} frames"
            );

            let mut rebuilt = epoch;
            rebuilt.blur_px = rebuilt.damaged_px + 1;
            assert_eq!(
                judge(&rebuilt),
                Verdict::FrostWork,
                "a frost rebuilt past the damage it serves must fail at {frames} frames"
            );
        }
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
            Verdict::DamageTotal,
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
