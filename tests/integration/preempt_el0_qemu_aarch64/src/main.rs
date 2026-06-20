//! `plans/PI.md` D2b-2b-A P-1 QEMU integration test: prove the production
//! generic-timer IRQ **involuntarily preempts** a runaway EL0 task on the
//! aarch64 `virt` board.
//!
//! On boot the test reads the GICv2 base + generic-timer rate from the embedded
//! `virt` device tree (`plans/PI.md` P3/P4), brings up the EL1 vectors + GICv2,
//! and installs a syscall-dispatch callback. It builds one hardware-isolated
//! EL0 address space from the `el0_spinner` `rxe` fixture through the production
//! capability-checked, audited `rustos_kernel_core::spawn_image` caller and
//! admits it as a resumable user kthread (`spawn_user_kthread`).
//!
//! It then arms the **production** preemption path verbatim (`AGENTS.md` §2.2 —
//! the same `rustos_arch_aarch64::preempt` surface the bin crate's
//! `arm_preemption` uses): registers a per-CPU `PreemptStorage`, installs an
//! EL0-preemption callback that suspends the running task back to the scheduler
//! via `reschedule_current(_, Yield)`, and starts the periodic generic timer.
//! EL0 already runs preemptible (`userentry`'s `SPSR_EL0T_PREEMPTIBLE`), so a
//! timer tick taken while the spinner is in EL0 traps to the `LOWER_IRQ` vector
//! and drives the preempt point.
//!
//! Driving the `step` loop dispatches the spinner into EL0, where it busy-loops
//! issuing **no** syscall. Because the loop never traps, the *only* thing that
//! can return control to the dispatcher before the spinner's final `exit` is an
//! involuntary timer preemption. The test PASSes once (a) the preempt callback
//! fired at least once **and** (b) the task — correctly resumed mid-loop after
//! each preemption — still completed its spin and exited. A preemption that
//! never fires (the `step` never returns) or a botched resume (the task never
//! exits) stalls the drain, so the run fails loudly — by a failure code or by
//! the harness `Outcome::Timeout` (`AGENTS.md` §7).

#![cfg_attr(itest_aarch64, no_std)]
#![cfg_attr(itest_aarch64, no_main)]
#![deny(missing_docs)]

#[cfg(all(feature = "test-hooks", not(debug_assertions)))]
compile_error!(
    "rustos-test-preempt-el0-qemu-aarch64: the `test-hooks` Cargo feature is a \
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
