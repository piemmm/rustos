//! The one step that installs x86_64's production trap callbacks.
//!
//! Hoisted out of the freestanding-only `crate::x86_64::arch_wrapper`
//! module — as `crate::riscv64_preempt_wiring` is for its port — so the
//! wiring carries a host regression test. The callbacks themselves are
//! architecture-neutral and shared by every port
//! (`crate::preempt_callbacks`); this module only names the x86_64 trap
//! slots they go into.
//!
//! x86_64 has no separate reschedule-IPI callback slot: its placement IPI
//! is delivered on its own vector and latches the need-resched there, so
//! only the ring-3 preempt point and the LAPIC-timer tick are installed
//! here.
//!
//! A forgotten install is a silent lost wakeup rather than a build
//! failure, so the host test below pins each slot's contents.

use crate::preempt_callbacks::{PREEMPT_CALLBACK, TIMER_CALLBACK};
use tairix_arch_x86_64::preempt;

/// Install every trap callback the x86_64 preemption surface forwards to.
///
/// Both slots are idempotent pointer stores, not one-shot slots, so a
/// re-call needs no fail-closed guard. Called in the kernel-core `Irq`
/// phase, before `init` drops to ring 3 with `IF` set — so the first tick
/// that can be *taken* already has a handler.
pub(crate) fn install_callbacks() {
    preempt::set_preempt_callback(PREEMPT_CALLBACK);
    preempt::set_timer_callback(TIMER_CALLBACK);
}

#[cfg(test)]
mod tests {
    use super::install_callbacks;
    use crate::preempt_callbacks::{installed, PREEMPT_CALLBACK, TIMER_CALLBACK};
    use tairix_arch_x86_64::preempt;

    /// Both slots must hold the *shared* kernel-core callback rather than
    /// a port-local restatement of it: a restated tick body is free to omit
    /// the deadline sweep or the stall sample and nothing else would notice.
    #[test]
    fn the_wiring_step_installs_the_shared_preempt_and_tick_callbacks() {
        install_callbacks();

        assert!(
            installed(preempt::preempt_callback(), PREEMPT_CALLBACK),
            "ring-3 preemption callback not installed"
        );
        assert!(
            installed(preempt::timer_callback(), TIMER_CALLBACK),
            "LAPIC-timer callback not installed"
        );
    }
}
