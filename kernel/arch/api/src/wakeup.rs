//! Pure arithmetic shared by every port's tickless one-shot combiner
//! (`AGENTS.md` §17.1, §2.21).
//!
//! A port programs its *single* physical one-shot timer to the earlier of
//! two pending events: the running task's preemption quantum
//! ([`SchedulerArch::set_preemption`](crate::SchedulerArch::set_preemption))
//! and the nearest pending blocking-wait deadline
//! ([`SchedulerArch::set_wakeup`](crate::SchedulerArch::set_wakeup)). The
//! per-CPU deadline *storage* and the actual timer programming are
//! genuinely target-divergent (register layout, MMIO, counter source) and
//! live in each `kernel/arch/<target>` port; the *combining math* below is
//! byte-for-byte identical across ports, so it lives here once rather than
//! being copied into three sibling preempt modules (`AGENTS.md` §2.2 /
//! §2.21).
//!
//! All three helpers are pure `const fn`s over plain integers, so they are
//! exercised by host unit tests with no timer hardware.

/// The earlier of two optional absolute monotonic-tick deadlines.
///
/// `None` means "no event of that kind pending". The result is the soonest
/// deadline either side carries, or `None` when neither does — exactly the
/// value the physical one-shot is armed to (`None` ⇒ disarm, `AGENTS.md`
/// §17.1: a CPU with nothing to preempt to and no armed wakeup takes no
/// timer interrupt).
#[must_use]
pub const fn earliest(quantum: Option<u64>, wakeup: Option<u64>) -> Option<u64> {
    match (quantum, wakeup) {
        (Some(a), Some(b)) => Some(if a <= b { a } else { b }),
        (Some(a), None) => Some(a),
        (None, b) => b,
    }
}

/// Relative ticks to arm a one-shot for the absolute deadline `target`
/// observed at counter value `now`.
///
/// Clamped to at least one tick so a deadline already at or in the past
/// arms the soonest possible interrupt rather than a zero-interval timer
/// that re-fires with no forward progress (`AGENTS.md` §2.9 — fail
/// closed). Saturating, so a `target` far in the future cannot wrap.
#[must_use]
pub const fn ticks_from_now(target: u64, now: u64) -> u64 {
    let delta = target.saturating_sub(now);
    if delta == 0 {
        1
    } else {
        delta
    }
}

/// Absolute counter ticks for an absolute monotonic-nanoseconds deadline
/// against a `hz` counter frequency (`deadline_ns * hz / 1e9`).
///
/// Computed in 128-bit space and saturated to [`u64::MAX`] so no realistic
/// deadline overflows, and `hz == 0` is treated as `1` so a malformed
/// frequency cannot divide-by-zero (`AGENTS.md` §2.9). This is the
/// `set_wakeup` half of the conversion the monotonic clock performs in the
/// other direction (`ticks * 1e9 / hz`), kept on the same one frequency
/// (`AGENTS.md` §2.4).
#[must_use]
pub const fn ns_to_ticks(deadline_ns: u64, hz: u64) -> u64 {
    let hz = if hz == 0 { 1 } else { hz };
    let ticks = (deadline_ns as u128 * hz as u128) / 1_000_000_000u128;
    if ticks > u64::MAX as u128 {
        u64::MAX
    } else {
        // The `> u64::MAX` guard above proves the value fits, so the
        // narrowing cast is lossless (`AGENTS.md` §2.11 — the cast cannot
        // truncate on this branch).
        #[allow(clippy::cast_possible_truncation)]
        let narrowed = ticks as u64;
        narrowed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn earliest_picks_the_soonest_pending_deadline() {
        assert_eq!(earliest(None, None), None);
        assert_eq!(earliest(Some(50), None), Some(50));
        assert_eq!(earliest(None, Some(70)), Some(70));
        // The quantum is sooner.
        assert_eq!(earliest(Some(40), Some(90)), Some(40));
        // The wakeup is sooner.
        assert_eq!(earliest(Some(90), Some(40)), Some(40));
        // A tie resolves to the same value either way.
        assert_eq!(earliest(Some(40), Some(40)), Some(40));
    }

    #[test]
    fn ticks_from_now_clamps_a_past_deadline_to_one() {
        assert_eq!(ticks_from_now(100, 40), 60);
        // Deadline exactly now arms the soonest tick, never zero.
        assert_eq!(ticks_from_now(40, 40), 1);
        // Deadline already in the past also arms one tick (fail closed).
        assert_eq!(ticks_from_now(10, 40), 1);
    }

    #[test]
    fn ns_to_ticks_converts_against_the_counter_frequency() {
        // 1 ms at 1 MHz is 1000 ticks.
        assert_eq!(ns_to_ticks(1_000_000, 1_000_000), 1_000);
        // 1 s at 24 MHz (a typical aarch64 CNTFRQ) is 24M ticks.
        assert_eq!(ns_to_ticks(1_000_000_000, 24_000_000), 24_000_000);
        // A zero frequency cannot divide by zero: it is treated as 1 Hz,
        // so the result is small but the call never traps (`AGENTS.md`
        // §2.9). The value is immaterial — a zero-frequency clock is a
        // malformed input the combiner never arms against in practice.
        assert_eq!(ns_to_ticks(1_000_000_000, 0), 1);
        // A pathologically large deadline saturates rather than wrapping.
        assert_eq!(ns_to_ticks(u64::MAX, 1_000_000_000), u64::MAX);
    }
}
