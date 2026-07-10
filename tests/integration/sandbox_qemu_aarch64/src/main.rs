//! S8b QEMU integration test: prove the `lib/sandbox` parser-sandbox seam
//! end to end on the aarch64 `virt` board
//! (`docs/src/security/sandbox.md`; `.junie/fstree-next-plan.md` S8b).
//!
//! On boot the test reads the GICv2 base + generic-timer rate from the
//! embedded `virt` device tree, brings up the EL1 vectors + GICv2, and
//! installs the **production** `KernelDispatchHook` through a
//! `DispatchCallbackSlot` — the production `LiveMemMap` producer for the
//! `rustos-rt` heap, a real `KernelProcessWait`, and a `ProgramRegistry`
//! carrying the fixture's three worker paths. The parent role of the
//! fixture program (`tests/integration/sandbox_program`) is spawned through
//! the production `InitSpawnCtx::spawn_driver_process` seam and drives the
//! seam over the real syscalls:
//!
//! * container and instruction decode of valid **and** malformed inputs
//!   through a genuinely sandboxed decode worker — its own binary spawned
//!   via `SpawnAttach::sandbox` over a pipe pair, branded capability-empty
//!   and confined to the sandbox allow-list by the S8a kernel primitive;
//! * real crash containment — a worker that exits without serving yields a
//!   typed `WorkerFailed`, a logged `EVENT_WORKER_CRASHED`, and a caller
//!   that goes on decoding through its healthy worker;
//! * the syscall wall from the inside — a probe worker's `fs_open` and
//!   `spawn` are refused inside the sandbox while its pipe reply crosses.
//!
//! The chassis reaps the parent through the wait producer's non-blocking
//! poll; PASS fires only on a parent exit of 0. Any misbehaviour surfaces
//! as a distinct failure finisher (the parent's diagnostic exit code is
//! folded into the finisher), and a wedged run times out through the
//! harness (fail-loud, `AGENTS.md` §7).

#![cfg_attr(itest_aarch64, no_std)]
#![cfg_attr(itest_aarch64, no_main)]
#![deny(missing_docs)]

#[cfg(all(feature = "test-hooks", not(debug_assertions)))]
compile_error!(
    "rustos-test-sandbox-qemu-aarch64: the `test-hooks` Cargo feature is a \
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
