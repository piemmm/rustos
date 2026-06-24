//! `plans/PI.md` stage RV-X1 QEMU integration test (riscv64 sibling of the
//! x86_64 X1 / aarch64 `SP2c` verticals): prove a **single** U-mode task can be
//! admitted as a *resumable user kthread* on the riscv64 `virt` board and
//! cooperatively park/resume under the live scheduler over the RV1 park-safe
//! trap path.
//!
//! On boot the test reads the generic-timer rate from the firmware device tree,
//! stands up an Sv39 address space that identity-maps the kernel and MMIO,
//! activates it (`satp`), and installs the trap vector + a syscall-dispatch
//! callback. It builds one hardware-isolated U-mode address space — its own
//! Sv39 page-table hierarchy — from the `rxe` fixture program through the
//! production capability-checked, audited `rustos_kernel_core::spawn_image`
//! caller, and admits it as a resumable user kthread via
//! `rustos_kernel_core::spawn_user_kthread`. The task's `pre_resume` hook
//! reactivates the task's own `satp` root (`paging::activate_user_root`, the
//! RV-X1 primitive) immediately before every switch-in. The cooperative
//! `Scheduler::step` loop drives it; the dispatch callback maps each
//! `yield`/`exit` `ecall` to `rustos_kernel_core::reschedule_current`, so the
//! task ping-pongs with the dispatcher through real U-mode↔kernel context
//! switches that park and resume mid-handler (the first exerciser of RV1's
//! park-safety on a user task).
//!
//! PASS once the task yielded its full count and exited. A wrong drain count,
//! an unexpected syscall, or a stall trips a distinct `SiFive` Test failure
//! finisher or times out, so the run fails loudly — by a failure code or by the
//! harness `Outcome::Timeout`.

#![cfg_attr(itest_riscv64, no_std)]
#![cfg_attr(itest_riscv64, no_main)]
#![deny(missing_docs)]

#[cfg(all(feature = "test-hooks", not(debug_assertions)))]
compile_error!(
    "rustos-test-spawn-el0-resume-qemu-riscv64: the `test-hooks` Cargo feature is a \
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
