//! The one step that installs aarch64's production trap callbacks.
//!
//! Hoisted out of the freestanding-only `crate::aarch64::gic_irq` module —
//! as `crate::riscv64_preempt_wiring` is for its port — so the wiring
//! carries a host regression test. The callbacks themselves are
//! architecture-neutral and shared by every port
//! (`crate::preempt_callbacks`); this module only names the aarch64 trap
//! slots they go into.
//!
//! A forgotten install is a silent lost wakeup rather than a build
//! failure, so the host test below pins each slot's contents.

#[cfg(any(freestanding, test))]
use crate::preempt_callbacks::{IPI_CALLBACK, PREEMPT_CALLBACK, TIMER_CALLBACK};
#[cfg(any(freestanding, test))]
use tairix_arch_aarch64::preempt;

/// Install every trap callback the aarch64 preemption surface forwards
/// to.
///
/// Called before the timer and the placement SGI are armed, so a
/// delivered trap always has a handler.
// The production caller is this port's freestanding `arm_preemption`, so a
// host build of the crate compiles the module for its test alone. Gated to
// match, rather than left to read as dead code on a host whose own ISA is
// this one — where it fails `-D warnings` while every other host passes.
#[cfg(any(freestanding, test))]
pub(crate) fn install_callbacks() {
    preempt::set_preempt_callback(PREEMPT_CALLBACK);
    preempt::set_timer_callback(TIMER_CALLBACK);
    preempt::set_ipi_callback(IPI_CALLBACK);
}

#[cfg(test)]
mod tests {
    use super::install_callbacks;
    use crate::preempt_callbacks::{installed, IPI_CALLBACK, PREEMPT_CALLBACK, TIMER_CALLBACK};
    use tairix_arch_aarch64::preempt;

    /// Each of the three slots must hold the *shared* kernel-core callback
    /// rather than a port-local restatement of it: a restated tick body is
    /// free to omit the deadline sweep or the stall sample and nothing else
    /// would notice.
    #[test]
    fn the_wiring_step_installs_the_shared_preempt_tick_and_ipi_callbacks() {
        install_callbacks();

        assert!(
            installed(preempt::preempt_callback(), PREEMPT_CALLBACK),
            "EL0 preemption callback not installed"
        );
        assert!(
            installed(preempt::timer_callback(), TIMER_CALLBACK),
            "generic-timer callback not installed"
        );
        assert!(
            installed(preempt::ipi_callback(), IPI_CALLBACK),
            "reschedule-IPI callback not installed"
        );
    }
}
