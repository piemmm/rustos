//! The frequency policy: how fast should this machine be running right now.
//!
//! Pure arithmetic over values the caller supplies, so the whole policy is
//! host-testable without a kernel, a clock, or a CPU. [`super::domain`] holds
//! the state it reads and decides when to ask.
//!
//! # Utilisation
//!
//! A CPU's utilisation is the fraction of the recent past it spent running.
//! The dispatch loop already brackets idle exactly — it parks in one place
//! and resumes in one place — so every span is wholly busy or wholly idle,
//! and [`fold`] mixes one such span into the filter, weighted by how long it
//! lasted. Three properties matter:
//!
//! * **It needs no periodic sampling, and a lazy read is exact.** Reading the
//!   filter folds the span since the last commit at the moment somebody asks,
//!   which is precisely what committing then reading would have produced. So
//!   an idle CPU's utilisation decays with nothing armed to make it, and the
//!   kernel stays tickless.
//! * **It carries the duty cycle across idle.** A task that runs 2 ms in
//!   every 100 ms holds its CPU near 2%, because the idle spans fold in as
//!   zero. So a low-duty background service does not read as a busy machine.
//! * **A full window of one state saturates it.** A window wholly busy reads
//!   1 and one wholly idle reads 0, whatever came before, so no history
//!   outlives its relevance.
//!
//! The blend is linear in the span's length rather than a true exponential,
//! which makes it cheap — one multiply and one divide, no repeated squaring
//! on a path every idle transition takes. The price is that it is not
//! composable: the same duty cycle switched at a coarse granularity and at a
//! fine one settle to slightly different values (a half-busy CPU switching
//! every 50 ms oscillates around a third to two thirds; one switching every
//! microsecond sits at a half). Both are monotone in the duty cycle and land
//! within a step or two of each other after quantisation, which is all a rate
//! decision needs — but nothing here may assume the filter is granularity
//! independent, because it is not.
//!
//! # Target
//!
//! `target = 1.25 × max × util`, quantised up to the mechanism's step and
//! clamped to its range. The quarter of headroom is the one Linux's
//! `schedutil` uses: a CPU that is *fully* busy at some rate has no way to
//! say how much faster it would like to be, so the governor must overshoot to
//! find out.
//!
//! # A wake is not a reason to go fast; a launch is
//!
//! Leaving idle says *something* happened, not how much: a CPU that wakes to
//! do a millisecond of work and parks again has not earned the top rate. An
//! earlier revision granted the maximum for a whole window on every such
//! wake, and because each wake pushed the deadline further out, any machine
//! waking more than ten times a second — an idle desktop with a compositor
//! and a clock — sat at its ceiling for ever at one percent load. Only the
//! filter can tell those apart, because only the filter measures the work.
//! So a wake grants no rate at all; it merely tells the governor to start
//! looking again ([`ACTIVE_REVIEW_NS`]).
//!
//! A program **launch** is the one case measurement cannot answer, and it is
//! the reason the boost still exists. The work is latency-critical from
//! before it has run an instruction, and most of what follows is waiting on
//! the volume the executable is read from — during which every CPU may be
//! idle and no utilisation accrues at all. So the kernel commits to the
//! maximum for one window when it starts an executable, and the filter takes
//! over from there. That is an explicit, bounded, infrequent event, not an
//! inference from a wake.
//!
//! It grants no authority a caller did not have: stamping it needs spawn
//! authority, and a principal that may start a program may equally pin the
//! clock by running work that genuinely deserves it.

use tairix_abi::cpufreq::CpuFreqLimits;

/// Fixed-point shift for a utilisation fraction: `1 << UTIL_SHIFT` is 1.0.
///
/// Sixteen bits resolve a utilisation to about 15 parts per million, far
/// finer than any step a real mechanism can deliver, and keeps every product
/// below well within a `u64`.
pub(super) const UTIL_SHIFT: u32 = 16;

/// Fully busy, in the [`UTIL_SHIFT`] fixed point.
pub(super) const UTIL_ONE: u64 = 1 << UTIL_SHIFT;

/// The interactive responsiveness window, in nanoseconds.
///
/// One constant serves three roles that are the same question asked three
/// ways — over what span is "how busy is this machine" a meaningful
/// question:
///
/// * the utilisation filter's time constant,
/// * how long a program launch holds the maximum rate, and
/// * how often the target may step back down.
///
/// A tenth of a second is under the threshold at which a person perceives a
/// delay, and four orders of magnitude above the cost of a mechanism round
/// trip, so a full descent from maximum to minimum costs one round trip per
/// step rather than a storm of them. A policy constant, not a capacity: it
/// paces decisions and bounds nothing.
pub(super) const RESPONSE_WINDOW_NS: u64 = 100_000_000;

/// How often the target is revisited while any CPU is running work, in
/// nanoseconds.
///
/// A CPU that stays busy produces no transition to observe, so the rise has
/// to be looked for rather than waited for. Four looks per window is the
/// coarsest cadence that still shows the ramp: with the headroom applied, the
/// filter must move several percent of full scale to shift the target by one
/// step, so looking much more often would find nothing changed, and looking
/// less often would jump straight from the floor to the ceiling and skip the
/// rates in between.
///
/// It bounds the climb to a handful of mechanism round trips rather than one
/// per step, and it arms nothing on a quiet machine: no CPU active means no
/// cadence.
pub(super) const ACTIVE_REVIEW_NS: u64 = RESPONSE_WINDOW_NS / 4;

/// The cadence must divide the window and be strictly finer than it, or the
/// ramp is either invisible or never reached.
const _: () = assert!(ACTIVE_REVIEW_NS > 0 && ACTIVE_REVIEW_NS < RESPONSE_WINDOW_NS);
const _: () = assert!(RESPONSE_WINDOW_NS.is_multiple_of(ACTIVE_REVIEW_NS));

/// Utilisation above which the ceiling is asked for outright, rather than
/// scaled up to.
///
/// The proportional rate with its headroom reaches the ceiling on its own at
/// four fifths of a core, which leaves a genuinely busy machine climbing
/// through intermediate rates it will not stay at. Past half a core the
/// workload has shown it wants the machine, so it is given it: an operator
/// decision that trades some power for the throughput and latency of the
/// range's top end.
pub(super) const BUSY_THRESHOLD: u64 = UTIL_ONE / 2;

/// Shortest time the ceiling is held once asked for, in nanoseconds.
///
/// Reaching the top and dropping straight off it again is the worst of both:
/// the workload pays a mechanism round trip in each direction and gets the
/// lower rate for the part of its burst that mattered. Holding for six tenths
/// of a second spans several bursts of a workload that is intermittent at the
/// filter's own granularity, so the rate stops flapping at the top of the
/// range. An operator decision, and a pacing constant rather than a capacity:
/// it delays a reduction and bounds nothing.
pub(super) const MAX_HOLD_NS: u64 = 600_000_000;

/// The hold must outlast the window the filter measures over, or a burst
/// would be re-measured as quiet before the hold it started could expire.
const _: () = assert!(MAX_HOLD_NS > RESPONSE_WINDOW_NS);

/// Headroom numerator and denominator applied to a utilisation-derived rate.
///
/// A CPU saturated at its current rate cannot report how much faster it
/// wanted to go, so the target overshoots by a quarter to find out — the
/// ratio `schedutil` uses.
const HEADROOM_NUM: u128 = 5;
const HEADROOM_DEN: u128 = 4;

/// Mix one wholly-busy or wholly-idle span into a utilisation filter.
///
/// `util` is the filter's value in [`UTIL_SHIFT`] fixed point, `span_ns` the
/// length of the span that just ended, and `busy` whether the CPU was running
/// throughout it. A span at least [`RESPONSE_WINDOW_NS`] long replaces the
/// filter outright — nothing older is still relevant — and a shorter one
/// contributes in proportion to its length.
#[must_use]
pub(super) const fn fold(util: u64, span_ns: u64, busy: bool) -> u64 {
    let weight = if span_ns > RESPONSE_WINDOW_NS {
        RESPONSE_WINDOW_NS
    } else {
        span_ns
    };
    let keep = RESPONSE_WINDOW_NS - weight;
    let sample = if busy { UTIL_ONE } else { 0 };
    // `util <= UTIL_ONE` and `keep <= RESPONSE_WINDOW_NS`, so the widest
    // product here is 2^16 · 10^8 — six orders of magnitude inside a `u64`.
    (util * keep + sample * weight) / RESPONSE_WINDOW_NS
}

/// The rate `limits` should deliver for a utilisation of `util`.
///
/// Applies the headroom, rounds *up* to a whole step so a target is one the
/// mechanism can actually deliver, and clamps into range. Rounding up rather
/// than down means a machine that wants 41% of its top rate is given the step
/// above, never the one below — under-delivering a rate the workload has
/// already demonstrated it needs is the more expensive mistake.
#[must_use]
pub(super) fn proportional_hz(limits: &CpuFreqLimits, util: u64) -> u64 {
    // 128-bit throughout: `max_hz` is declared by the mechanism, so it is not
    // this arithmetic's place to assume it is small.
    let scaled = u128::from(limits.max_hz) * u128::from(util) * HEADROOM_NUM
        / (u128::from(UTIL_ONE) * HEADROOM_DEN);
    let step = u128::from(limits.step_hz);
    let min = u128::from(limits.min_hz);
    let max = u128::from(limits.max_hz);
    // Round up to a step boundary measured from `min`, so both endpoints of
    // the range stay reachable however the step divides it.
    let stepped = if scaled <= min {
        min
    } else {
        let above = scaled - min;
        min + above.div_ceil(step) * step
    };
    let clamped = if stepped > max { max } else { stepped };
    // Bounded by `max_hz` above, which came from a `u64`.
    u64::try_from(clamped).unwrap_or(limits.max_hz)
}

/// The rate to ask for, given the machine's busiest CPU and whether a launch
/// boost is still live.
///
/// `boosted` is decided by the caller against its own clock, so this stays
/// pure. Two cases ask for the ceiling outright rather than scaling up to it:
/// a launch, because no measurement yet describes the program that is
/// starting, and a machine past [`BUSY_THRESHOLD`], because it has already
/// shown it wants the whole range.
#[must_use]
pub(super) fn target_hz(limits: &CpuFreqLimits, peak_util: u64, boosted: bool) -> u64 {
    if boosted || peak_util > BUSY_THRESHOLD {
        limits.max_hz
    } else {
        proportional_hz(limits, peak_util)
    }
}

/// The rate to publish, given what [`target_hz`] wants and what is already
/// published.
///
/// A reduction off the ceiling is refused until the ceiling has been held for
/// [`MAX_HOLD_NS`]; `at_max_since` is when the published rate last became the
/// maximum, and is ignored when the published rate is not the maximum. A
/// *rise* is never delayed — the hold exists to stop the rate flapping off the
/// top, not to slow it reaching it.
#[must_use]
pub(super) fn held_target_hz(
    limits: &CpuFreqLimits,
    want_hz: u64,
    published_hz: u64,
    at_max_since: u64,
    now_ns: u64,
) -> u64 {
    // One definition of "is the ceiling held": a second spelling here could
    // disagree with the deadline the waiter parks on.
    if hold_expiry(limits, want_hz, published_hz, at_max_since, now_ns).is_some() {
        limits.max_hz
    } else {
        want_hz
    }
}

/// When a hold on the ceiling expires, or [`None`] when nothing is held.
///
/// The waiter parks on this: while the ceiling is held nothing else can move
/// the published rate, so the expiry is the only event worth waking for.
#[must_use]
pub(super) fn hold_expiry(
    limits: &CpuFreqLimits,
    want_hz: u64,
    published_hz: u64,
    at_max_since: u64,
    now_ns: u64,
) -> Option<u64> {
    let expiry = at_max_since.saturating_add(MAX_HOLD_NS);
    if published_hz == limits.max_hz && want_hz < limits.max_hz && now_ns < expiry {
        Some(expiry)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{fold, proportional_hz, target_hz, RESPONSE_WINDOW_NS, UTIL_ONE};
    use tairix_abi::cpufreq::CpuFreqLimits;

    /// A Pi 4B: 600 MHz to 1.5 GHz in 100 MHz steps.
    fn pi4() -> CpuFreqLimits {
        CpuFreqLimits::new(600_000_000, 1_500_000_000, 100_000_000).expect("a real range")
    }

    #[test]
    fn a_full_window_of_running_saturates_the_filter() {
        assert_eq!(fold(0, RESPONSE_WINDOW_NS, true), UTIL_ONE);
        // Longer than the window is still the whole window: nothing older
        // than one window survives.
        assert_eq!(fold(0, RESPONSE_WINDOW_NS * 7, true), UTIL_ONE);
    }

    #[test]
    fn a_full_window_of_idle_empties_the_filter() {
        assert_eq!(fold(UTIL_ONE, RESPONSE_WINDOW_NS, false), 0);
        assert_eq!(fold(UTIL_ONE, RESPONSE_WINDOW_NS * 7, false), 0);
    }

    #[test]
    fn the_filter_never_leaves_its_range() {
        // A convex combination of the filter and a sample, both in range,
        // cannot escape it — the invariant the fixed-point product width
        // argument depends on.
        let mut util = 0;
        for step in 0..200u64 {
            util = fold(util, step * 1_000_000, step % 3 == 0);
            assert!(util <= UTIL_ONE, "step {step} left the range: {util}");
        }
    }

    #[test]
    fn a_longer_span_of_one_state_moves_the_filter_further() {
        // The monotonicity a rate decision rests on: more evidence of idling
        // lowers the filter, and more evidence of running raises it, with no
        // span able to move it the wrong way.
        let mut previous_idle = UTIL_ONE + 1;
        let mut previous_busy = 0;
        for tenths in 0..=10 {
            let span = RESPONSE_WINDOW_NS * tenths / 10;
            let idle = fold(UTIL_ONE, span, false);
            let busy = fold(0, span, true);
            assert!(idle < previous_idle, "idling {span} ns did not lower it");
            assert!(busy >= previous_busy, "running {span} ns lowered it");
            previous_idle = idle;
            previous_busy = busy;
        }
        assert_eq!(previous_idle, 0, "a whole window of idle empties it");
        assert_eq!(previous_busy, UTIL_ONE, "a whole window of work fills it");
    }

    #[test]
    fn folding_in_pieces_decays_rather_than_matching_one_long_span() {
        // Stated as a test because it is a real property of the cheap linear
        // blend, not an accident to be relied on either way: folding the same
        // elapsed time in many small pieces decays geometrically and so lands
        // *above* one long fold. Nothing in the governor may assume the two
        // agree — what it relies on is that a lazy read equals a commit at the
        // same instant, which is one fold either way.
        let whole = fold(UTIL_ONE, RESPONSE_WINDOW_NS / 2, false);
        let mut pieces = UTIL_ONE;
        for _ in 0..50 {
            pieces = fold(pieces, RESPONSE_WINDOW_NS / 100, false);
        }
        assert!(
            pieces > whole,
            "piecewise {pieces} should decay above the single fold {whole}"
        );
        // Both still read as "about half a window of idling has passed",
        // which is the accuracy a quantised rate decision needs.
        assert!(pieces < UTIL_ONE * 3 / 4 && whole > UTIL_ONE / 4);
    }

    #[test]
    fn a_duty_cycle_reads_as_that_duty_cycle() {
        // 2 ms of work in every 100 ms is a background service, not a busy
        // machine: the filter must say so, or a governor would run a quiet
        // desktop flat out.
        let mut util = 0;
        for _ in 0..100 {
            util = fold(util, 2_000_000, true);
            util = fold(util, 98_000_000, false);
        }
        let percent = util * 100 / UTIL_ONE;
        assert!(percent <= 5, "a 2% duty cycle read as {percent}%");
    }

    #[test]
    fn a_saturated_machine_asks_for_everything() {
        assert_eq!(proportional_hz(&pi4(), UTIL_ONE), 1_500_000_000);
    }

    #[test]
    fn an_idle_machine_asks_for_the_minimum() {
        assert_eq!(proportional_hz(&pi4(), 0), 600_000_000);
    }

    #[test]
    fn a_partly_busy_machine_asks_for_a_whole_step_in_range() {
        let limits = pi4();
        for percent in 0..=100u64 {
            let hz = proportional_hz(&limits, UTIL_ONE * percent / 100);
            assert!(
                (limits.min_hz..=limits.max_hz).contains(&hz),
                "{percent}% left the range: {hz}"
            );
            assert_eq!(
                (hz - limits.min_hz) % limits.step_hz,
                0,
                "{percent}% asked for {hz}, not a step boundary"
            );
        }
    }

    #[test]
    fn the_headroom_overshoots_a_measured_rate() {
        // Half-busy on a 1.5 GHz part is 750 MHz of work; the quarter of
        // headroom asks for 937.5 MHz, which rounds up to the 1 GHz step.
        assert_eq!(proportional_hz(&pi4(), UTIL_ONE / 2), 1_000_000_000);
    }

    #[test]
    fn the_target_rounds_up_rather_than_down() {
        // 41% of 1.5 GHz with headroom is 768.75 MHz: between the 700 and
        // 800 MHz steps. Under-delivering a rate the workload has already
        // shown it needs is the worse error.
        assert_eq!(proportional_hz(&pi4(), UTIL_ONE * 41 / 100), 800_000_000);
    }

    #[test]
    fn a_launch_boost_asks_for_the_maximum_however_idle_the_machine_looks() {
        let limits = pi4();
        assert_eq!(target_hz(&limits, 0, true), limits.max_hz);
        assert_eq!(target_hz(&limits, 0, false), limits.min_hz);
    }

    #[test]
    fn a_low_duty_cycle_asks_for_the_minimum() {
        // The reported defect, at the policy level: an idle desktop showing a
        // live monitor sits at about one percent of a core, and must ask for
        // the floor. It already did — every load under about a third asks for
        // the minimum once the headroom and the clamp are applied — which is
        // why no utilisation threshold was added: the rate was pinned by a
        // wake boost that never let this arithmetic run.
        let limits = pi4();
        for percent in [1u64, 2, 5, 10] {
            let util = UTIL_ONE * percent / 100;
            assert_eq!(
                proportional_hz(&limits, util),
                limits.min_hz,
                "{percent}% of a core asked for more than the floor"
            );
        }
    }

    #[test]
    fn an_absurd_ceiling_cannot_overflow_the_arithmetic() {
        // `max_hz` is declared by the mechanism, so the policy must not
        // assume it describes real silicon.
        let wild = CpuFreqLimits::new(1, u64::MAX, 1).expect("a legal if absurd range");
        assert_eq!(proportional_hz(&wild, UTIL_ONE), u64::MAX);
        assert_eq!(proportional_hz(&wild, 0), 1);
    }

    #[test]
    fn a_fixed_rate_mechanism_is_always_asked_for_its_one_rate() {
        let fixed = CpuFreqLimits::new(1_000_000_000, 1_000_000_000, 100_000_000)
            .expect("one-rate mechanism");
        assert_eq!(proportional_hz(&fixed, 0), 1_000_000_000);
        assert_eq!(proportional_hz(&fixed, UTIL_ONE), 1_000_000_000);
    }
}
