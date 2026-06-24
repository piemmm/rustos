//! wasm32 timer programming ("timer programming").
//!
//! Implements the Arch HAL [`Timer`](rustos_arch_api::Timer) surface for
//! wasm32 over the cooperative `requestAnimationFrame` loop wired in
//! [`crate::preempt`]. The HAL handle is the architecture-neutral half
//! of the timer path: it installs the one scheduler-tick callback and
//! dispatches a tick to it. Requesting the next animation frame stays in
//! [`crate::preempt`] — it is host-binding work with no
//! architecture-neutral shape — and
//! [`crate::preempt::on_animation_frame`] dispatches each frame's tick
//! through [`Timer::dispatch_tick`](rustos_arch_api::Timer::dispatch_tick),
//! so the callback invoke lives in exactly one place.
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

    fn arm_oneshot(&self, _ticks_from_now: u64) {
        // wasm32 cannot pre-empt a running module with a hardware timer:
        // it runs to completion on the host's JavaScript turn. Preemption
        // is driven through the host's equivalent yield facility — the
        // self-sustaining `requestAnimationFrame` loop
        // ([`crate::preempt::on_animation_frame`]) — which is the
        // carve-out for a target that cannot deliver an asynchronous
        // timer interrupt. The scheduler's quantum deadline has no LAPIC/
        // generic-timer analogue here, so arming a one-shot is a no-op:
        // the frame loop already re-enters the scheduler each frame, and
        // a CPU-bound task yields cooperatively at the frame boundary.
    }

    fn disarm(&self) {
        // No-op for the same reason as [`Self::arm_oneshot`]: the
        // cooperative frame loop is the host yield facility (wasm
        // carve-out) and is not gated on a per-quantum arm/disarm.
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
        // on the shared lock so the suites do not race (no flaky tests).
        let _guard = crate::preempt::test_state_lock();
        conformance::run_all(&TimerHal::new());
        let dynamic: &dyn Timer = &TimerHal::new();
        conformance::run_all(dynamic);
    }
}
