//! P-5 QEMU integration test (`plans/PI.md`): prove the
//! fully-preemptive kernel delivers a timer interrupt **while a long in-kernel
//! kthread runs**, yet never preempts the kernel itself.
//!
//! Where the `preempt_el0_qemu_*` verticals prove that a runaway **EL0** task
//! *is* involuntarily preempted, this vertical proves the dual property the
//! serial-stall saga turned on (2026-06-23 amendment): a
//! busy **in-kernel** kthread that issues no `yield` and no syscall still takes
//! the generic-timer IRQ *during* its span — the EL1 IRQ path runs and accounts
//! the tick — but because the interrupt was taken from EL1 the running task is
//! **not** rescheduled (the kernel is non-preemptible): the
//! interrupt returns to the same task, which runs to its voluntary completion.
//!
//! On boot the test reads the GICv2 bases + generic-timer rate from the
//! embedded `virt` device tree, brings up the EL1 vectors + GICv2, registers
//! the **production** `rustos_arch_aarch64::preempt` surface verbatim
//! (a per-CPU `PreemptStorage`, the EL0-preemption callback,
//! a timer-tick callback, and the enabled generic-timer PPI), builds a live
//! eevdf `Scheduler`, and admits one in-kernel kthread. The kthread arms the
//! generic-timer one-shot and busy-loops; with device IRQs enabled at the PE
//! (`exceptions::enable_irq`, the aarch64 backing of
//! `KernelArch::set_device_irqs(true)`), the timer fires mid-loop and the tick
//! callback records it.
//!
//! It PASSes once (a) at least one timer tick was taken during the busy span,
//! (b) the EL0-preemption callback fired zero times (the kernel was never
//! preempted), and (c) the kthread resumed correctly after the interrupt and
//! ran to completion. Under the old cooperative dispatch loop (device IRQs
//! masked across the whole task run) no tick would ever be taken and the
//! kthread would spin forever, so the run fails loudly — by a failure code or
//! the harness `Outcome::Timeout`.

#![cfg_attr(itest_aarch64, no_std)]
#![cfg_attr(itest_aarch64, no_main)]
#![deny(missing_docs)]

#[cfg(all(feature = "test-hooks", not(debug_assertions)))]
compile_error!(
    "rustos-test-preempt-inkernel-qemu-aarch64: the `test-hooks` Cargo feature is a \
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
