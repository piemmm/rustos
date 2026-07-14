//! Regression QEMU integration test: prove a **non-timer** interrupt taken
//! from EL0 involuntarily preempts a **sole**, CPU-bound EL0 task once the
//! per-CPU need-resched latch is set.
//!
//! This is the regression test for the interrupt-return-to-EL0 need-resched
//! fix. Its sibling `preempt_el0_qemu_aarch64` proves the *timer* preempts a
//! *contended* runaway task; this one proves the behaviour the fix adds: a
//! device or IPI interrupt — anything that is **not** the preemption timer —
//! taken while a lone CPU-bound task runs (so the tickless scheduler never
//! armed the preemption one-shot) still forces the task back into the
//! scheduler when the interrupt made higher-priority work runnable.
//!
//! On boot the test reads the GICv2 base + timer rate from the embedded `virt`
//! device tree, brings up the EL1 vectors + GICv2, installs a syscall-dispatch
//! callback, and builds one hardware-isolated EL0 address space from the
//! `el0_spinner` `rxe` fixture through the production capability-checked,
//! audited `rustos_kernel_core::spawn_image` caller. It then wires the
//! **production** preemption surface: the per-CPU `PreemptStorage`, the
//! latch-gated EL0-preemption callback (reschedules only when
//! `take_preempt_pending` is set — the production `production_preempt_dispatch`
//! shape), and the reschedule-IPI callback that latches need-resched
//! (`note_preempt_tick` — the production `production_ipi_dispatch` shape). It
//! spawns the spinner as the **sole** runnable task, so the scheduler keeps
//! the generic timer disarmed: the timer never fires.
//!
//! Just before entering EL0 the spinner's kthread sends **itself** a
//! reschedule SGI (`gic::send_sgi`). IRQs are masked at EL1, so the SGI stays
//! pending until `enter_user`'s `eret` unmasks IRQ in EL0, whereupon it is
//! taken on the first EL0 instruction (`LOWER_IRQ`). Its handler latches
//! need-resched; the fix then drives the EL0 preempt point on return-to-EL0
//! for that non-timer interrupt, and the latch-gated preempt callback
//! reschedules. The test PASSes once the preempt callback rescheduled at least
//! once **and** the spinner — resumed mid-loop — still completed and exited.
//! Before the fix the SGI only latched (the preempt point ran solely for the
//! timer PPI) and the sole spinner ran unpreempted forever: the `step` never
//! returns and the harness reports `Outcome::Timeout` (fail-loud).

#![cfg_attr(itest_aarch64, no_std)]
#![cfg_attr(itest_aarch64, no_main)]
#![deny(missing_docs)]

#[cfg(all(feature = "test-hooks", not(debug_assertions)))]
compile_error!(
    "rustos-test-preempt-wake-qemu-aarch64: the `test-hooks` Cargo feature is a \
     debug-only test affordance and must not be enabled in release builds. \
     See AGENTS.md §1 (no hacks) and §5.4.5 (fail closed)."
);

#[cfg(all(itest_aarch64, feature = "test-hooks"))]
extern crate alloc;

#[cfg(all(itest_aarch64, feature = "test-hooks"))]
mod kernel;

// --- Stub when the test-hooks feature is off ----------------------
#[cfg(all(itest_aarch64, not(feature = "test-hooks")))]
#[no_mangle]
pub extern "C" fn kernel_main(_dtb: u64) -> ! {
    loop {
        // SAFETY: `wfe` is a well-defined parked-CPU hint on aarch64.
        unsafe {
            core::arch::asm!("wfe", options(nomem, nostack, preserves_flags));
        }
    }
}

#[cfg(all(itest_aarch64, not(feature = "test-hooks")))]
#[panic_handler]
fn panic_stub(_info: &core::panic::PanicInfo<'_>) -> ! {
    loop {
        // SAFETY: same as above.
        unsafe {
            core::arch::asm!("wfe", options(nomem, nostack, preserves_flags));
        }
    }
}

// --- Host stub -----------------------------------------------------
#[cfg(not(itest_aarch64))]
fn main() {}
