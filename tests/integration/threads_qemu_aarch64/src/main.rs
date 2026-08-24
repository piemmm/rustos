//! `SP11c` QEMU integration test: prove the demand-grown user stack end to
//! end on the aarch64 `virt` board (`docs/src/architecture/memory.md` §7c).
//!
//! On boot the test reads the GICv2 base + generic-timer rate from the
//! embedded `virt` device tree, brings up the EL1 vectors + GICv2, and
//! installs the **production** `KernelDispatchHook` through a
//! `DispatchCallbackSlot` — with the production `LiveMemMap` producer
//! backing both `mem_map` and the stack-growth fault path, a real
//! `KernelProcessWait`, a `ProgramRegistry` carrying the three child roles
//! (their numeric parameters derived from the one shared `spawn_layout`
//! policy), and the production user-fault resolver bound to the same slot.
//! The parent role of the fixture program
//! (`tests/integration/threads_program`) is spawned through the
//! production `InitSpawnCtx::spawn_driver_process` seam; it drives the
//! children through the production `spawn` + `wait` syscalls:
//!
//! * `grow` recurses far past the eagerly committed stack top — the run
//!   only completes if the fault-driven growth path backs every page — and
//!   verifies each frame's bytes survived, exiting 0;
//! * `limit` lowers its own `StackBytes` soft bound through `rlimit_set`
//!   and recurses past it — the parent observes exit 139 through `wait`
//!   (the audited `stack_limit` fault-kill);
//! * `guard` reads the unmapped guard page below the reserved span — the
//!   parent observes exit 139 (the guard stays the structural defence).
//!
//! The chassis reaps the parent through the wait producer's non-blocking
//! poll; PASS fires only on a parent exit of 0. Any child or parent
//! misbehaviour surfaces as a distinct failure finisher (the parent's
//! diagnostic exit code is folded into the finisher), and a wedged run
//! times out through the harness (fail-loud, `AGENTS.md` §7).

#![cfg_attr(itest_aarch64, no_std)]
#![cfg_attr(itest_aarch64, no_main)]
#![deny(missing_docs)]

#[cfg(all(feature = "test-hooks", not(debug_assertions)))]
compile_error!(
    "tairix-test-threads-qemu-aarch64: the `test-hooks` Cargo feature is a \
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

// --- Host stub -----------------------------------------------------
#[cfg(not(itest_aarch64))]
fn main() {}
