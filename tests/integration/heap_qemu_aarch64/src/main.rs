//! PI P6e-3b prerequisite QEMU integration test: prove the `rustos-rt`
//! `mem_map`-backed global allocator works end to end in an EL0 process on the
//! aarch64 `virt` board (`plans/PI.md`).
//!
//! On boot the test reads the GICv2 base + generic-timer rate from the embedded
//! `virt` device tree (`plans/PI.md` P3/P4), brings up the EL1 vectors + GICv2,
//! and installs a syscall-dispatch callback and a fault handler. It builds one
//! hardware-isolated EL0 address space — its own stage-1 page-table hierarchy —
//! from the `rxe` fixture program through the production capability-checked,
//! audited `rustos_kernel_core::spawn_image` caller, **retains it live**, and
//! installs a `rustos_kernel_core::MemMap` producer backed by
//! `rustos_kernel_mem::map_anonymous` / `unmap_anonymous` over that space and
//! its frame pool. It admits the program as a resumable user kthread
//! (`spawn_user_kthread`, `plans/SPAWN.md` SP2) and drains the cooperative
//! `step` loop; the dispatch callback routes the program's allocator-issued
//! `mem_map` / `mem_unmap` `svc`s through the producer.
//!
//! The fixture program (`tests/integration/heap_program`) Box-allocates, grows
//! a `Vec` across several pages, reallocates after freeing, verifies every
//! value, and exits 0 — a clean `exit(0)` is the PASS signal. Any shortfall (a
//! non-zero exit code, an unexpected syscall, or a fault) trips a distinct
//! failure finisher or times out, so the run fails loudly.

#![cfg_attr(itest_aarch64, no_std)]
#![cfg_attr(itest_aarch64, no_main)]
#![deny(missing_docs)]

#[cfg(all(feature = "test-hooks", not(debug_assertions)))]
compile_error!(
    "rustos-test-heap-qemu-aarch64: the `test-hooks` Cargo feature is a \
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
