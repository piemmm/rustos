//! The riscv64 production preemption/IPI trap callbacks and the one step
//! that installs them.
//!
//! Hoisted out of the freestanding-only `crate::riscv64` port module — as
//! `crate::riscv64_plic_irq` is — so the wiring carries a host regression
//! test. The three callbacks only reach architecture-neutral `kernel/core`
//! entry points, so they build and run on the CI host.
//!
//! Installing them is one step rather than three call sites because a hart
//! that parks in `wfi` sleeps through any wake source it left un-enabled:
//! `wfi` resumes only for *locally* enabled interrupts (it ignores the
//! global `sstatus.SIE`). A forgotten source is therefore a silent lost
//! wakeup, not a compile error, and the riscv64 reschedule IPI was missing
//! for exactly that reason.

use tairix_arch_api::CpuId;
use tairix_arch_riscv64::preempt;

/// Reschedules on return-to-U-mode when this hart owes one — i.e. the
/// per-hart need-resched latch is set by a quantum expiry, a reschedule
/// IPI, or a device interrupt that woke a higher-priority task. An
/// interrupt that woke nothing returns straight to U-mode with no
/// gratuitous context switch.
///
/// A false return (no resumable user kthread published on `cpu`) makes a
/// stray invocation a no-op rather than an unsound switch.
pub(crate) extern "C" fn preempt_dispatch(cpu: CpuId) {
    let _ = tairix_kernel_core::preempt_current(cpu);
}

/// Runs on **every** supervisor-timer tick, U-mode or idle S-mode.
///
/// Latches the tick as this hart's pending preemption, runs the
/// blocking-wait timed-wake sweep (so a finite `hw_tree_wait` timeout
/// fires even with every task parked), and samples the stall watchdog —
/// a tick still fires on a hart looping without returning to the
/// scheduler, so this is where a soft lockup becomes observable. All
/// three are pure accounting and never context-switch, which is what
/// makes them safe on a tick taken in S-mode.
pub(crate) extern "C" fn tick_dispatch(cpu: CpuId) {
    tairix_kernel_core::note_preempt_tick(cpu);
    tairix_kernel_core::timed_wake_sweep();
    tairix_kernel_core::check_stall(cpu);
}

/// Runs on a delivered reschedule IPI (supervisor software interrupt).
///
/// Latching the need-resched makes a U-mode task on the targeted hart
/// yield at its next syscall boundary, so cross-hart placement is honoured
/// promptly on a busy hart too. Pure accounting — the context switch is
/// [`preempt_dispatch`]'s U-mode-only job.
pub(crate) extern "C" fn ipi_dispatch(cpu: CpuId) {
    tairix_kernel_core::note_preempt_tick(cpu);
}

/// Install every trap callback the riscv64 preemption surface forwards to.
///
/// Called before the sources are unmasked, so a delivered trap always has
/// a handler.
pub(crate) fn install_callbacks() {
    preempt::set_preempt_callback(preempt_dispatch);
    preempt::set_timer_callback(tick_dispatch);
    preempt::set_ipi_callback(ipi_dispatch);
}

#[cfg(test)]
mod tests {
    use super::{install_callbacks, ipi_dispatch, preempt_dispatch, tick_dispatch};
    use tairix_arch_riscv64::preempt;

    /// Every source the idle park can be woken by must have its callback
    /// installed by the one wiring step.
    ///
    /// The reschedule IPI is the one this pins hardest: it was absent from
    /// the production riscv64 boot path, so `send_ipi` raised `sip.SSIP` on
    /// a hart that had neither enabled the source nor installed a handler —
    /// the bit latched unacknowledged for the rest of the boot and `wfi`
    /// never resumed on it.
    #[test]
    fn the_wiring_step_installs_the_preempt_tick_and_ipi_callbacks() {
        use tairix_arch_api::CpuId;

        install_callbacks();

        let installed = |slot: Option<extern "C" fn(CpuId)>, want: extern "C" fn(CpuId)| {
            slot.is_some_and(|got| core::ptr::fn_addr_eq(got, want))
        };

        assert!(
            installed(preempt::preempt_callback(), preempt_dispatch),
            "U-mode preemption callback not installed"
        );
        assert!(
            installed(preempt::timer_callback(), tick_dispatch),
            "supervisor-timer callback not installed"
        );
        assert!(
            installed(preempt::ipi_callback(), ipi_dispatch),
            "reschedule-IPI callback not installed"
        );
    }
}
