//! `plans/PI.md` stage RV-X2 QEMU integration test (riscv64 sibling of the
//! x86_64 X2 / aarch64 `SP2c` verticals): prove **two** U-mode tasks timeshare
//! one hart as *resumable user kthreads* on the riscv64 `virt` board under the
//! live scheduler over the RV1 park-safe trap path.
//!
//! On boot the test reads the generic-timer rate from the firmware device tree,
//! installs the trap vector + a syscall-dispatch callback, and builds **two**
//! hardware-isolated U-mode address spaces — each its own Sv39 page-table
//! hierarchy (two `PageTablePool`s + a shared frame pool) —
//! from the `rxe` fixture program through the production capability-checked,
//! audited `tairix_kernel_core::spawn_image` caller, admitting each as a
//! resumable user kthread via `tairix_kernel_core::spawn_user_kthread`. Each
//! task's `pre_resume` hook reactivates *its own* `satp` root
//! (`paging::activate_user_root`, the RV-X1 primitive) immediately before every
//! switch-in. The cooperative `Scheduler::step` loop drives both; the dispatch
//! callback maps each `yield`/`exit` `ecall` to
//! `tairix_kernel_core::reschedule_current`, so the two tasks ping-pong with the
//! dispatcher through real U-mode↔kernel context switches that park and resume
//! mid-handler, each on its own kernel stack (the RV1 park-safe path).
//!
//! PASS once both tasks yielded their full count and exited. A wrong drain
//! count, an unexpected syscall, or a stall trips a distinct `SiFive` Test
//! failure finisher or times out, so the run fails loudly — by a failure code
//! or by the harness `Outcome::Timeout`.

#![cfg_attr(itest_riscv64, no_std)]
#![cfg_attr(itest_riscv64, no_main)]
#![deny(missing_docs)]

#[cfg(all(feature = "test-hooks", not(debug_assertions)))]
compile_error!(
    "tairix-test-spawn-el0-timeshare-qemu-riscv64: the `test-hooks` Cargo feature is a \
     debug-only test affordance and must not be enabled in release builds. \
     See AGENTS.md §1 (no hacks) and §5.4.5 (fail closed)."
);

#[cfg(all(itest_riscv64, feature = "test-hooks"))]
extern crate alloc;

#[cfg(all(itest_riscv64, feature = "test-hooks"))]
mod kernel;

// --- Stub when the test-hooks feature is off ----------------------
#[cfg(all(itest_riscv64, not(feature = "test-hooks")))]
#[no_mangle]
pub extern "C" fn kernel_main(_hartid: u64, _dtb: u64) -> ! {
    loop {
        // SAFETY: `wfi` is a well-defined parked-hart hint on riscv64.
        unsafe {
            core::arch::asm!("wfi", options(nomem, nostack, preserves_flags));
        }
    }
}

#[cfg(all(itest_riscv64, not(feature = "test-hooks")))]
#[panic_handler]
fn panic_stub(_info: &core::panic::PanicInfo<'_>) -> ! {
    loop {
        // SAFETY: same as above.
        unsafe {
            core::arch::asm!("wfi", options(nomem, nostack, preserves_flags));
        }
    }
}

// --- Host stub -----------------------------------------------------
#[cfg(not(itest_riscv64))]
fn main() {}
