//! CCOMPAT stage CC3 QEMU integration test: spawn a separately-linked C-ABI
//! program (crt0 + abi-sys) on the aarch64 `virt` board and assert its `exit`
//! syscall carries the decimal argument it was spawned with.
//!
//! On boot the test builds a stage-1 address space that identity-maps the
//! kernel and MMIO (EL1), activates it, installs the EL1 vector table and a
//! dispatch callback, then calls the production capability-checked, audited
//! spawn caller (`tairix_kernel_core::spawn_and_enter`, gated on
//! `CAP_PROC_SPAWN`) to materialise the program's EL0 image — built from the
//! `rxe` blob the build script produced — and `eret` into it. The program
//! parses `argv[1]` and returns it; crt0 routes the return through the `exit`
//! syscall, whose `svc` traps back through the kernel EL1 vector to the
//! dispatch callback, which asserts the code equals the spawned argument
//! before the ARM semihosting PASS finisher.

#![cfg_attr(itest_aarch64, no_std)]
#![cfg_attr(itest_aarch64, no_main)]
#![deny(missing_docs)]

#[cfg(all(feature = "test-hooks", not(debug_assertions)))]
compile_error!(
    "tairix-test-spawn-program-qemu-aarch64: the `test-hooks` Cargo feature is a \
     debug-only test affordance and must not be enabled in release builds. \
     See AGENTS.md §1 (no hacks) and §5.4.5 (fail closed)."
);

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
