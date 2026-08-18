//! riscv64 QEMU integration test: a U-mode program cannot steer the kernel's
//! per-hart identity through the psABI thread pointer, and its own `tp`
//! survives every trap.
//!
//! `tp` (x4) is an ordinary unprivileged register U-mode code writes freely,
//! *and* the riscv64 port's per-hart kernel anchor — `smp::current_hartid` and
//! the Arch-HAL per-CPU slice read the running hart's identity out of it. A trap
//! vector that left `tp` alone would therefore hand the kernel a caller-chosen
//! CPU identity on every `ecall`, letting a task steer the kernel onto another
//! core's per-CPU state (its resume handle, dispatch slot, live address space).
//! The vector closes that by spilling the user's `tp` into the trap frame and
//! reloading the kernel's from the task's trap anchor before any other register
//! is touched (`kernel/arch/riscv64/src/trap.s`).
//!
//! On boot the test builds an Sv39 address space that identity-maps the kernel
//! and MMIO, activates it, installs the trap vector and a dispatch callback,
//! then calls the production capability-checked, audited spawn caller
//! (`tairix_kernel_core::spawn_and_enter`, gated on `CAP_PROC_SPAWN`) to
//! materialise the adversarial fixture's U-mode image and `sret` into it.
//!
//! The fixture (`tests/integration/tp_probe_program`) runs two rounds: write a
//! hostile sentinel into `tp`, `yield`, then check its own value came back. Two
//! distinct sentinels are used so a vector that merely zeroed `tp`, or restored
//! a stale value, fails too.
//!
//! PASS requires all three properties together: the dispatch callback read the
//! *true* boot hart id on every `ecall` (the security property), the fixture
//! exited 0 (its `tp` round-tripped both times — the per-thread-pointer
//! property thread-local storage rests on), and both rounds actually ran. Each
//! shortfall writes a distinct `SiFive` Test failure finisher, and a guest that
//! never reaches its exit trips the harness timeout, so the run fails loudly
//! either way.

#![cfg_attr(itest_riscv64, no_std)]
#![cfg_attr(itest_riscv64, no_main)]
#![deny(missing_docs)]

#[cfg(all(feature = "test-hooks", not(debug_assertions)))]
compile_error!(
    "tairix-test-tp-isolation-qemu-riscv64: the `test-hooks` Cargo feature is a \
     debug-only test affordance and must not be enabled in release builds. \
     See AGENTS.md §1 (no hacks) and §5.4.5 (fail closed)."
);

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
