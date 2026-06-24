//! SPAWN stage `SP2c` QEMU integration test: prove two EL0 user tasks timeshare
//! one CPU under the live scheduler on the aarch64 `virt` board.
//!
//! On boot the test reads the GICv2 base + generic-timer rate from the embedded
//! `virt` device tree (`plans/PI.md` P3/P4), brings up the EL1 vectors + GICv2,
//! and installs a syscall-dispatch callback. It then builds **two**
//! hardware-isolated EL0 address spaces — each its own stage-1 page-table
//! hierarchy — from the same `rxe` fixture program through the production
//! capability-checked, audited `rustos_kernel_core::spawn_image` caller, and
//! admits each as a resumable user kthread (`spawn_user_kthread`, `plans/SPAWN.md`
//! SP2) whose `pre_resume` hook reactivates that task's page-table root before
//! every switch-in, so the two tasks stay isolated.
//!
//! Driving the cooperative `step` loop interleaves the two runnable tasks. Each
//! task's `yield`/`exit` `svc` traps back to the dispatch callback, which maps
//! it to `rustos_kernel_core::reschedule_current` — suspending the running task
//! back to the dispatcher exactly as the production bin-crate callback does. The
//! test PASSes once both tasks have yielded their full count and exited (no task
//! left live), proving a real EL0→EL0 context switch under the live scheduler.
//! A regression (a switch that never resumes, a task that never runs) stalls the
//! drain or trips a distinct failure finisher, so the run fails loudly — by a
//! failure code or by the harness `Outcome::Timeout`.

#![cfg_attr(itest_aarch64, no_std)]
#![cfg_attr(itest_aarch64, no_main)]
#![deny(missing_docs)]

#[cfg(all(feature = "test-hooks", not(debug_assertions)))]
compile_error!(
    "rustos-test-spawn-el0-timeshare-qemu-aarch64: the `test-hooks` Cargo feature is a \
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
