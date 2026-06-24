//! `plans/PI.md` stage RV-X4 QEMU integration test (riscv64 sibling of
//! `wait_qemu_aarch64` / `_x86_64`): prove a parent U-mode task can `wait` on,
//! reap, and read back the exit code of its own child under the live scheduler
//! on the `virt` board.
//!
//! On boot the test reads the generic-timer rate from the firmware device
//! tree, installs the trap vector + a syscall-dispatch callback, and builds a
//! **child** and a **parent** hardware-isolated Sv39 U-mode address space from
//! the `rxe` fixture program (`tests/integration/wait_program`, built PIE in
//! both roles) through the production capability-checked, audited
//! `rustos_kernel_core::spawn_image` caller, admitting each as a resumable user
//! kthread via `rustos_kernel_core::spawn_user_kthread`. It records the
//! parent/child link with a `rustos_kernel_core::KernelProcessWait` producer and
//! drives the cooperative `Scheduler::step` loop. The dispatch callback routes
//! the child's `exit` and the parent's `wait`/`exit` `ecall`s through the
//! producer + `rustos_kernel_core::reschedule_current`: the producer parks the
//! parent until the child is reapable (the RV1 mid-handler-park-safe path),
//! then the kernel copies the reaped exit code out to the parent's `status`
//! pointer.
//!
//! PASS once the parent reaped the child, read back the agreed code, and exited
//! 0. A wrong code, a missing reap, an unexpected syscall, or a stall trips a
//! distinct `SiFive` Test failure finisher or times out, so the run fails
//! loudly — by a failure code or by the harness `Outcome::Timeout`.

#![cfg_attr(itest_riscv64, no_std)]
#![cfg_attr(itest_riscv64, no_main)]
#![deny(missing_docs)]

#[cfg(all(feature = "test-hooks", not(debug_assertions)))]
compile_error!(
    "rustos-test-wait-qemu-riscv64: the `test-hooks` Cargo feature is a \
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
