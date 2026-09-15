//! Instants and durations, kept apart.
//!
//! The kernel's wait takes a **duration**: how long to park from now. Most
//! interactive loops hold an **instant** instead — when the next frame or tick
//! falls due — and handing one to the other is a mistake nothing catches,
//! because both are a `u64` of nanoseconds and the wrong one is merely a very
//! long wait rather than an error. An animated viewer parked for the machine's
//! whole uptime that way, and showed one frame.
//!
//! So the two parks in the app shell are named for what they take — `park_for`
//! a duration, `park_until` an instant — and the conversion between them lives
//! here rather than beside them, because that shell is compiled only for the
//! freestanding targets and arithmetic nothing can test is arithmetic nothing
//! checked.

/// How long is left before `deadline_ns`, as a park budget.
///
/// A deadline already reached leaves nothing to wait for, so the park returns
/// at once and the caller acts on what is due. A caller whose deadline never
/// advances would then spin, which is that caller's defect to fix rather than
/// something to hide behind a floor here.
#[must_use]
pub const fn remaining_ns(deadline_ns: u64, now_ns: u64) -> u64 {
    deadline_ns.saturating_sub(now_ns)
}

#[cfg(test)]
mod tests {
    use super::remaining_ns;

    #[test]
    fn a_future_deadline_becomes_the_budget_left_before_it() {
        assert_eq!(remaining_ns(1_000, 400), 600);
        assert_eq!(remaining_ns(u64::MAX, 0), u64::MAX, "no deadline at all");
    }

    #[test]
    fn a_deadline_already_reached_leaves_no_budget() {
        assert_eq!(remaining_ns(1_000, 1_000), 0, "due exactly now");
        assert_eq!(remaining_ns(1_000, 5_000), 0, "overrun, never wrapped");
        assert_eq!(remaining_ns(0, u64::MAX), 0);
    }

    #[test]
    fn an_instant_is_never_mistaken_for_a_budget() {
        // The defect this split exists to stop: a caller one second into a
        // frame's wait must be left a frame's worth of budget, not a reading
        // the size of the machine's uptime.
        let uptime = 3_600 * 1_000_000_000;
        let frame = 16_666_666;
        assert_eq!(remaining_ns(uptime + frame, uptime), frame);
    }
}
