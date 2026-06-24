//! PI P10 chunk 5d-0-ii (b′)-2 QEMU integration test: prove an EL0 driver can
//! map a granted device MMIO window at runtime via `abi-v1` `mmio_map` on the
//! aarch64 `virt` board (`plans/PI.md` 5d-0-ii (b′)).
//!
//! On boot the test reads the GICv2 base + generic-timer rate from the embedded
//! `virt` device tree (`plans/PI.md` P3/P4), brings up the EL1 vectors + GICv2,
//! and installs a syscall-dispatch callback. It builds one hardware-isolated
//! EL0 address space — its own stage-1 page-table hierarchy — from the `rxe`
//! fixture program through the production capability-checked, audited
//! `rustos_kernel_core::spawn_image` caller, wraps it in the **production**
//! `rustos_kernel_mem::LiveSpace`, and admits the program through
//! `rustos_kernel_core::spawn_user_kthread_with_stack_live` so the retained
//! live space is published on the per-CPU live-space slot while the program
//! runs (exactly the production aarch64 spawn path, `plans/PI.md`
//! 5d-0-ii (b′)-2).
//!
//! The dispatch callback routes the program's `mmio_map` `svc` through the
//! retained space — `rustos_kernel_core::with_current_live_space` +
//! `rustos_kernel_mem::LiveSpace::map_device_window` — for the granted
//! virtio-MMIO transport window. The program maps the window, reads the
//! device's `MagicValue` register back through the returned base, and exits 0;
//! the callback reports exit 0 as PASS. Any shortfall (a refused map, the wrong
//! register value, an unexpected syscall) trips a distinct failure finisher or
//! times out, so the run fails loudly.
//!
//! The registry-backed grant owner-check is host-proven in
//! `kernel/core`; this vertical proves the retained-space device-window mapping
//! mechanism on the board, the part QEMU `virt` can carry honestly.

#![cfg_attr(itest_aarch64, no_std)]
#![cfg_attr(itest_aarch64, no_main)]
#![deny(missing_docs)]

#[cfg(all(feature = "test-hooks", not(debug_assertions)))]
compile_error!(
    "rustos-test-mmio-map-qemu-aarch64: the `test-hooks` Cargo feature is a \
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
