//! wasm32 timer programming (`AGENTS.md` §17.2 "timer programming").
//!
//! Implements the Arch HAL [`Timer`](rustos_arch_api::Timer) surface for
//! wasm32 over the cooperative `requestAnimationFrame` loop wired in
//! [`crate::preempt`]. The HAL handle is the architecture-neutral half
//! of the timer path: it installs the one scheduler-tick callback and
//! dispatches a tick to it. Requesting the next animation frame stays in
//! [`crate::preempt`] — it is host-binding work with no
//! architecture-neutral shape (`AGENTS.md` §2.4) — and
//! [`crate::preempt::on_animation_frame`] dispatches each frame's tick
//! through [`Timer::dispatch_tick`](rustos_arch_api::Timer::dispatch_tick),
//! so the callback invoke lives in exactly one place (`AGENTS.md` §2.2).
//!
//! The handle is zero-sized: the callback lives in [`crate::preempt`]'s
//! lock-free static (the frame loop's source of truth), so the handle
//! forwards to it on both the wasm target and the host build. Unlike the
//! bare-metal ports, wasm32's [`crate::preempt::on_animation_frame`] is
//! host-callable, so the handle and the frame loop must share that one
//! store rather than the handle keeping a private host cell.

use rustos_arch_api::{CpuId, TickFn, Timer};

/// wasm32 implementation of the Arch HAL timer-programming surface.
///
/// Zero-sized — the callback lives in [`crate::preempt`]'s static, which
/// is host-visible, so the handle forwards to it on every target.
#[derive(Debug, Default)]
pub struct TimerHal;

impl TimerHal {
    /// Construct the wasm32 timer handle.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Timer for TimerHal {
    fn set_tick_callback(&self, callback: TickFn) {
        crate::preempt::set_tick_callback(callback);
    }

    fn tick_callback(&self) -> Option<TickFn> {
        crate::preempt::tick_callback()
    }

    fn dispatch_tick(&self, cpu: CpuId) -> bool {
        match self.tick_callback() {
            Some(cb) => {
                cb(cpu);
                true
            }
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustos_arch_api::timer::conformance;

    #[test]
    fn passes_timer_conformance() {
        // The handle forwards to `preempt`'s process-global callback
        // static, which the `preempt` host tests also mutate; serialise
        // on the shared lock so the suites do not race (`AGENTS.md` §7 —
        // no flaky tests).
        let _guard = crate::preempt::test_state_lock();
        conformance::run_all(&TimerHal::new());
        let dynamic: &dyn Timer = &TimerHal::new();
        conformance::run_all(dynamic);
    }
}
