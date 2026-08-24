//! M1 QEMU integration test: prove the production demand-paged file-mapping
//! path end to end on the riscv64 `virt` board — the riscv64 twin of
//! `file_map_qemu_aarch64` (`docs/src/architecture/memory.md` §7o).
//!
//! On boot the test reads the timebase from the live device tree OpenSBI
//! passed in `a1`, installs the S-mode trap vector, and installs the
//! **production** `KernelDispatchHook` through a `DispatchCallbackSlot` —
//! with the production `LiveMemMap` producer wired for both `mem_map` and
//! `file_map`, a read-only in-guest filesystem double serving one fixture
//! file, a real `KernelProcessWait`, a `ProgramRegistry` carrying the three
//! child roles, and the production user-fault resolver bound to the same
//! slot. The parent role of the fixture program
//! (`tests/integration/file_map_program`) is spawned through the production
//! `InitSpawnCtx::spawn_driver_process` seam; it drives the children through
//! the production `spawn` + `wait` syscalls:
//!
//! * `verify` demand-faults the mapping's first byte, an interior page, and
//!   the end-of-file straddle page (file bytes + zero fill verified), proves
//!   the mapping survives `fs_close`, hands an **untouched** mapped page to
//!   `fs_open` as its path buffer (the syscall copy-path fault-resolution
//!   proof), unmaps, and exits 0;
//! * `wild` maps, unmaps, and reads the unmapped base — the parent observes
//!   exit 139 through `wait` (the fault-kill path);
//! * `store` maps, faults a page resident, and stores to it — the read-only
//!   write-fault gate kills it, exit 139.
//!
//! The chassis reaps the parent through the wait producer's non-blocking poll;
//! PASS fires only on a parent exit of 0. Any child or parent misbehaviour
//! surfaces as a distinct failure finisher (the parent's diagnostic exit code
//! is folded into the finisher), and a wedged run times out through the harness
//! (fail-loud).

#![cfg_attr(itest_riscv64, no_std)]
#![cfg_attr(itest_riscv64, no_main)]
#![deny(missing_docs)]

#[cfg(all(feature = "test-hooks", not(debug_assertions)))]
compile_error!(
    "tairix-test-file-map-qemu-riscv64: the `test-hooks` Cargo feature is a \
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
