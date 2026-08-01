//! STRESSTEST `ST2` QEMU integration test: prove the `mem_pin` /
//! `mem_unpin` pair end to end on the aarch64 `virt` board
//! (`plans/STRESSTEST.md` ST2).
//!
//! On boot the test reads the GICv2 base + generic-timer rate from the
//! embedded `virt` device tree, brings up the EL1 vectors + GICv2, and
//! installs the **production** `KernelDispatchHook` through a
//! `DispatchCallbackSlot` — with the production `LiveMemMap` producer
//! backing `mem_map`, a real `KernelProcessWait`, and a `ProgramRegistry`
//! carrying the three child roles (the pinned budget derived from the one
//! shared `spawn_layout` policy). The parent role of the fixture program
//! (`tests/integration/mem_pin_program`) is spawned through the production
//! `InitSpawnCtx::spawn_driver_process` seam; it drives the children
//! through the production `spawn` + `wait` syscalls:
//!
//! * `deny` holds no `CAP_MEM_PIN` — its `mem_pin` is refused
//!   `PermissionDenied` at the dispatcher gate (the audited capability
//!   check, end to end through the real trap) while the ungated
//!   `mem_unpin` still answers success;
//! * `pin` lowers its own `pinned-memory-bytes` bound through
//!   `rlimit_set`, pins itself twice (idempotent success), sees an
//!   over-budget `mem_map` refused `OutOfRange` and a within-budget one
//!   succeed, unpins, and sees the formerly refused map succeed — the
//!   budget binds exactly while pinned;
//! * `child` is spawned by the parent *after the parent pinned itself*
//!   under a lowered bound: the child inherits the lowered limit but never
//!   the pin mark, so its over-budget map succeeds — pinning is
//!   process-scoped state a spawn never inherits.
//!
//! The chassis reaps the parent through the wait producer's non-blocking
//! poll; PASS fires only on a parent exit of 0. Any child or parent
//! misbehaviour surfaces as a distinct failure finisher (the parent's
//! diagnostic exit code is folded into the finisher), and a wedged run
//! times out through the harness (fail-loud).

#![cfg_attr(itest_aarch64, no_std)]
#![cfg_attr(itest_aarch64, no_main)]
#![deny(missing_docs)]

#[cfg(all(feature = "test-hooks", not(debug_assertions)))]
compile_error!(
    "tairix-test-mem-pin-qemu-aarch64: the `test-hooks` Cargo feature is a \
     debug-only test affordance and must not be enabled in release builds \
     (fail closed, never ship a test hook)."
);

#[cfg(all(itest_aarch64, feature = "test-hooks"))]
extern crate alloc;

#[cfg(all(itest_aarch64, feature = "test-hooks"))]
mod kernel;

// The migration driver's give-up policy. Compiled for the freestanding
// migration kernel body that uses it, and for host `cargo test`, which runs
// its unit tests without needing that body. Gated on the exact conditions the
// consumer (`kernel::…`) needs so the host bin build never carries it unused.
#[cfg(any(test, all(itest_aarch64, feature = "test-hooks", migration_smp)))]
mod progress;

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

// --- Host stub ----------------------------------------------------------
//
// On the host (`cargo build --workspace`, clippy, fmt) the freestanding
// kernel body is not compiled, so this inert `main` keeps the crate
// building under the host tooling. It performs no I/O.
#[cfg(not(itest_aarch64))]
fn main() {}
