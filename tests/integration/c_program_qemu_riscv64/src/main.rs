//! CCOMPAT stage CC5 QEMU integration test: spawn a genuinely C-compiled
//! program (clang object + crt0 + abi-sys staticlib) on the riscv64 `virt`
//! board and assert it exits `99` — i.e. every abi-v1 C-header check and both
//! syscall round-trips it performs succeed.
//!
//! On boot the test builds an Sv39 address space that identity-maps the kernel
//! and MMIO, activates it, installs the trap vector and a dispatch callback,
//! then calls the production capability-checked, audited spawn caller
//! (`rustos_kernel_core::spawn_and_enter`, gated on `CAP_PROC_SPAWN`) to
//! materialise the program's U-mode image — built from the `rxe` blob the
//! build script produced — and `sret` into it. The C program checks a Time64
//! value across the boundaries, an ipc header, and a sysinfo header, then
//! calls `cap_query` and `clock_get`; the dispatch callback services those two
//! syscalls (returning a known answer / sentinel) and finally asserts the
//! `exit` code is `99` before the `SiFive` Test PASS finisher.

#![cfg_attr(itest_riscv64, no_std)]
#![cfg_attr(itest_riscv64, no_main)]
#![deny(missing_docs)]

#[cfg(all(feature = "test-hooks", not(debug_assertions)))]
compile_error!(
    "rustos-test-c-program-qemu-riscv64: the `test-hooks` Cargo feature is a \
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
