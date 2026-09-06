//! The one step that installs riscv64's production trap callbacks.
//!
//! Hoisted out of the freestanding-only `crate::riscv64` port module — as
//! `crate::riscv64_plic_irq` is — so the wiring carries a host regression
//! test. The callbacks themselves are architecture-neutral and shared by
//! every port (`tairix_kernel_core::traps`); this module only names the
//! riscv64 trap slots they go into.
//!
//! Installing them is one step rather than three call sites because a hart
//! that parks in `wfi` sleeps through any wake source it left un-enabled:
//! `wfi` resumes only for *locally* enabled interrupts (it ignores the
//! global `sstatus.SIE`). A forgotten source is therefore a silent lost
//! wakeup, not a compile error, and the riscv64 reschedule IPI was missing
//! for exactly that reason.

use tairix_arch_riscv64::preempt;

/// Install every trap callback the riscv64 preemption surface forwards to.
///
/// Called before the sources are unmasked, so a delivered trap always has
/// a handler.
pub(crate) fn install_callbacks() {
    preempt::set_preempt_callback(tairix_kernel_core::on_user_preempt_point);
    preempt::set_timer_callback(tairix_kernel_core::on_timer_tick);
    preempt::set_ipi_callback(tairix_kernel_core::on_reschedule_ipi);
}

#[cfg(test)]
mod tests {
    use super::install_callbacks;
    use tairix_arch_riscv64::preempt;

    /// Every source the idle park can be woken by must have its callback
    /// installed by the one wiring step, and each must be the *shared*
    /// kernel-core callback rather than a port-local restatement of it.
    ///
    /// The reschedule IPI is the one this pins hardest: it was absent from
    /// the production riscv64 boot path, so `send_ipi` raised `sip.SSIP` on
    /// a hart that had neither enabled the source nor installed a handler —
    /// the bit latched unacknowledged for the rest of the boot and `wfi`
    /// never resumed on it. The timer slot pins the second half: a port
    /// that installs its own tick body can silently omit the blocking-wait
    /// deadline sweep.
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
            "U-mode preemption callback not installed"
        );
        assert!(
            installed(preempt::timer_callback(), tairix_kernel_core::on_timer_tick),
            "supervisor-timer callback not installed"
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
