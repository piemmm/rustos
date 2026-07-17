//! CCOMPAT stage CC3 QEMU integration test: spawn a separately-linked C-ABI
//! program (crt0 + abi-sys) on the riscv64 `virt` board and assert its `exit`
//! syscall carries the decimal argument it was spawned with.
//!
//! On boot the test builds an Sv39 address space that identity-maps the kernel
//! and MMIO, activates it, installs the trap vector and a dispatch callback,
//! then calls the production capability-checked, audited spawn caller
//! (`tairix_kernel_core::spawn_and_enter`, gated on `CAP_PROC_SPAWN`) to
//! materialise the program's U-mode image — built from the `rxe` blob the
//! build script produced — and `sret` into it. The program parses `argv[1]`
//! and returns it; crt0 routes the return through the `exit` syscall, whose
//! `ecall` traps back through the kernel S-mode vector to the dispatch
//! callback, which asserts the code equals the spawned argument before the
//! `SiFive` Test PASS finisher.

#![cfg_attr(itest_riscv64, no_std)]
#![cfg_attr(itest_riscv64, no_main)]
#![deny(missing_docs)]

#[cfg(all(feature = "test-hooks", not(debug_assertions)))]
compile_error!(
    "tairix-test-spawn-program-qemu-riscv64: the `test-hooks` Cargo feature is a \
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
