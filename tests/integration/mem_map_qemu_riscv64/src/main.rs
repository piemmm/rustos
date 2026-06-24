//! SPAWN stage `SP5b-2` QEMU integration test (riscv64 sibling): prove a
//! U-mode process can obtain and release anonymous `RW` memory at runtime via
//! `abi-v1` on the riscv64 `virt` board (`plans/SPAWN.md` SP5).
//!
//! On boot the test stands up an Sv39 address space that identity-maps the
//! kernel and MMIO, activates it (`satp`), installs the trap vector, a
//! syscall-dispatch callback, and a fault handler. It builds one
//! hardware-isolated U-mode address space — its own Sv39 page-table hierarchy —
//! from the `rxe` fixture program through the production capability-checked,
//! audited `rustos_kernel_core::spawn_image` caller, **retains it live**, and
//! installs a `rustos_kernel_core::MemMap` producer backed by
//! `rustos_kernel_mem::map_anonymous` / `unmap_anonymous` over that space and
//! its frame pool. It then `sret`s into the program directly through
//! `EnterUser::enter_user`; the dispatch callback routes the program's
//! `mem_map` / `mem_unmap` `ecall`s through the producer.
//!
//! The program maps an anonymous region (FIXED), writes and reads back a
//! pattern, unmaps it, then touches the released range — which must raise a
//! page fault the fault handler reports as PASS. Any shortfall (the program
//! returning a failure exit code, an unexpected syscall, or no fault) trips a
//! distinct `SiFive` Test failure finisher or times out, so the run fails
//! loudly — by a failure code or by the harness `Outcome::Timeout`.

#![cfg_attr(itest_riscv64, no_std)]
#![cfg_attr(itest_riscv64, no_main)]
#![deny(missing_docs)]

#[cfg(all(feature = "test-hooks", not(debug_assertions)))]
compile_error!(
    "rustos-test-mem-map-qemu-riscv64: the `test-hooks` Cargo feature is a \
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
