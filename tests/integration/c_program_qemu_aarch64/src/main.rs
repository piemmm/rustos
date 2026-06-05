//! CCOMPAT stage CC5 QEMU integration test: spawn a genuinely C-compiled
//! program (clang object + crt0 + abi-sys staticlib) into EL0 on the aarch64
//! `virt` board and assert it exits `99` — i.e. every abi-v1 C-header check and
//! both syscall round-trips it performs succeed.
//!
//! On boot the test enables `CPACR_EL1.FPEN`, builds a stage-1 address space
//! that identity-maps the kernel and MMIO (EL1), activates it, installs the EL1
//! vector table and a dispatch callback, then calls the production
//! capability-checked, audited spawn caller (`rustos_kernel_core::spawn_and_enter`,
//! gated on `CAP_PROC_SPAWN`) to materialise the program's EL0 image — built
//! from the `rxe` blob the build script produced — and `eret` into it. The C
//! program checks a Time64 value across the §21 boundaries, an ipc header, and
//! a sysinfo header, then calls `cap_query` and `clock_get`; the dispatch
//! callback services those two syscalls (returning a known answer / sentinel)
//! and finally asserts the `exit` code is `99` before the ARM semihosting PASS
//! finisher.

#![cfg_attr(itest_aarch64, no_std)]
#![cfg_attr(itest_aarch64, no_main)]
#![deny(missing_docs)]

#[cfg(all(feature = "test-hooks", not(debug_assertions)))]
compile_error!(
    "rustos-test-c-program-qemu-aarch64: the `test-hooks` Cargo feature is a \
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
