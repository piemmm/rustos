//! USERS `U4` QEMU integration test: prove on the aarch64 `virt` board that
//! a **service account's compiled capability ceiling binds a lying
//! manifest** end to end (`plans/USERS.md` U4).
//!
//! On boot the test reads the GICv2 base + generic-timer rate from the
//! embedded `virt` device tree, brings up the EL1 vectors + GICv2, and
//! installs the **production** `KernelDispatchHook` through a
//! `DispatchCallbackSlot` — with the real compiled system identity table
//! (`rustos_kernel_core::system_identity_table`, the same table the boot
//! `sec` phase installs) resolving spawn-as-user switches, the seeded
//! hardware tree backing `hw_tree_read`, and a `ProgramRegistry` whose one
//! `svc` row deliberately requests devmgr's ceiling **plus every sibling
//! service's defining capability**. The parent role of the fixture program
//! (`tests/integration/service_ceiling_program`) is spawned through the
//! production `InitSpawnCtx::spawn_driver_process` seam holding
//! `CAP_PROC_SPAWN` + `CAP_SPAWN_AS_USER`; it switches `svc` into the
//! devmgr account through the production `spawn` syscall. Running **as
//! devmgr**, `svc` proves the `ceiling ∩ manifest` intersection at the
//! audited dispatcher gate through real traps:
//!
//! * its own `CAP_SYSINFO_HW`-gated `hw_tree_read` succeeds — the account's
//!   genuine grant survives the intersection;
//! * `spawn_as` is refused `PermissionDenied` — neither `CAP_PROC_SPAWN`
//!   nor login's `CAP_SPAWN_AS_USER` survives, so the identity switch fails
//!   closed;
//! * `users_db_read` is refused `PermissionDenied` — login's
//!   `CAP_USERS_READ` was stripped;
//! * `seat_switch` is refused `PermissionDenied` — seatmgr's
//!   `CAP_SEAT_ADMIN` was stripped;
//! * `sysinfo_introspect` is refused `PermissionDenied` — sysinfod's
//!   `CAP_SYSINFO_INTROSPECT` was stripped.
//!
//! The chassis reaps the parent through the wait producer's non-blocking
//! poll; PASS fires only on a parent exit of 0. Any misbehaviour surfaces
//! as a distinct failure finisher (the parent's diagnostic exit code is
//! folded into the finisher), and a wedged run times out through the
//! harness (fail-loud).

#![cfg_attr(itest_aarch64, no_std)]
#![cfg_attr(itest_aarch64, no_main)]
#![deny(missing_docs)]

#[cfg(all(feature = "test-hooks", not(debug_assertions)))]
compile_error!(
    "rustos-test-service-ceiling-qemu-aarch64: the `test-hooks` Cargo feature \
     is a debug-only test affordance and must not be enabled in release \
     builds (fail closed, never ship a test hook)."
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

// --- Host stub ----------------------------------------------------------
//
// On the host (`cargo build --workspace`, clippy, fmt) the freestanding
// kernel body is not compiled, so this inert `main` keeps the crate
// building under the host tooling. It performs no I/O.
#[cfg(not(itest_aarch64))]
fn main() {}
