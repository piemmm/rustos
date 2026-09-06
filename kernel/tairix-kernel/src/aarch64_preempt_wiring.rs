//! The one step that installs aarch64's production trap callbacks.
//!
//! Hoisted out of the freestanding-only `crate::aarch64::gic_irq` module —
//! as `crate::riscv64_preempt_wiring` is for its port — so the wiring
//! carries a host regression test. The callbacks themselves are
//! architecture-neutral and shared by every port
//! (`tairix_kernel_core::traps`); this module only names the aarch64 trap
//! slots they go into.
//!
//! A forgotten install is a silent lost wakeup rather than a build
//! failure, so the host test below pins each slot's contents.

use tairix_arch_aarch64::preempt;

/// Install every trap callback the aarch64 preemption surface forwards
/// to.
///
/// Called before the timer and the placement SGI are armed, so a
/// delivered trap always has a handler.
pub(crate) fn install_callbacks() {
    preempt::set_preempt_callback(tairix_kernel_core::on_user_preempt_point);
    preempt::set_timer_callback(tairix_kernel_core::on_timer_tick);
    preempt::set_ipi_callback(tairix_kernel_core::on_reschedule_ipi);
}

#[cfg(test)]
mod tests {
    use super::install_callbacks;
    use tairix_arch_aarch64::preempt;

    /// Each of the three slots must hold the *shared* kernel-core callback
    /// rather than a port-local restatement of it: a restated tick body is
    /// free to omit the deadline sweep or the stall sample and nothing else
    /// would notice.
    #[test]
    fn the_wiring_step_installs_the_shared_preempt_tick_and_ipi_callbacks() {
        use tairix_arch_api::CpuId;

        install_callbacks();

        let installed = |slot: Option<extern "C" fn(CpuId)>, want: extern "C" fn(CpuId)| {
            slot.is_some_and(|got| core::ptr::fn_addr_eq(got, want))
        };

        assert!(
            installed(
                preempt::preempt_callback(),
                tairix_kernel_core::on_user_preempt_point
            ),
            "EL0 preemption callback not installed"
        );
        assert!(
            installed(preempt::timer_callback(), tairix_kernel_core::on_timer_tick),
            "generic-timer callback not installed"
        );
        assert!(
            installed(
                preempt::ipi_callback(),
                tairix_kernel_core::on_reschedule_ipi
            ),
            "reschedule-IPI callback not installed"
        );
    }
}
