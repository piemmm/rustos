//! `plans/USB.md` U1 QEMU integration test: prove the kernel **driver-unload**
//! mechanism end to end on the aarch64 `virt` board.
//!
//! The build script compiles the pure-Rust driver-stub program
//! (`tests/integration/driver_register_program`) position-independent and
//! wraps it as the signed payload of a `kind = UserSpace` driver image. On
//! boot the test discovers the board from the embedded `virt` device tree,
//! enables the identity MMU + EL1 vectors, builds the live kernel registries
//! (frame allocator, scheduler, capability table, address-space registry, IRQ
//! table), and autoloads the stub through the production
//! `DeviceManager::autoload` + `SpawnDriverLoader` + `InitCtxDriverProcessSpawn`
//! over `Aarch64ProcessSpawn::spawn_with` — the driver is admitted **Ready**
//! and its capability record + address-space-registry entry minted.
//!
//! It then drives the production driver-unload seam,
//! `InitSpawnCtx::terminate_driver_process` (the mechanism the driver-store
//! server runs for `StoreRequest::Unload`), and asserts the driver task was
//! reaped (the scheduler's live-task count drops 1 → 0) and its capability
//! record and address-space entry reclaimed; a second unload of the now-gone
//! handle must fail closed with `NotFound` (the teardown is idempotent). PASS
//! once teardown reclaimed everything and the idempotent miss was observed;
//! any shortfall writes a distinct failure finisher (fail-loud).

#![cfg_attr(itest_aarch64, no_std)]
#![cfg_attr(itest_aarch64, no_main)]
#![deny(missing_docs)]

#[cfg(all(feature = "test-hooks", not(debug_assertions)))]
compile_error!(
    "tairix-test-driver-unload-qemu-aarch64: the `test-hooks` Cargo feature is a \
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
