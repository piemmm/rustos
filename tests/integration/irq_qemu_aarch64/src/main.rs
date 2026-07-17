//! WIRING Stage W3-B QEMU integration test: a device IRQ routed through
//! GICv2 SPI target-routing and the EL1 IRQ path reaches a Rust handler
//! and drives the kernel/irq mask-before-wake path on the aarch64 `virt`
//! board — the EL1/SPI analogue of `tests/integration/irq_qemu_x86_64`.
//!
//! On boot the test installs the EL1 vectors, brings up the GICv2, builds
//! a kernel-neutral `tairix_kernel_irq::IrqTable`, binds the PL031 RTC's
//! GICv2 SPI line (INTID 34), installs a set-once device-IRQ dispatcher
//! (`tairix_arch_aarch64::exceptions::set_device_irq_dispatch`) that
//! forwards the line to `IrqTable::fire` over a `GicController` bridge,
//! routes that SPI to CPU 0 (`gic::route_spi` → `GICD_ITARGETSR`), arms
//! the RTC match interrupt, and unmasks IRQs. When the RTC fires, the GIC
//! delivers the SPI to EL1, the dispatcher masks the line and sets the
//! wait flag, and the main loop observes `WaitStep::Ready`. It then
//! re-reads the GIC enable bit and asserts the line is masked — the
//! mask-before-wake evidence (`docs/src/security/irq.md`) — before
//! reporting PASS via ARM semihosting. A regression that fails to route,
//! deliver, or mask never reaches PASS, so the run times out (the
//! documented fail-loud behaviour).

#![cfg_attr(itest_aarch64, no_std)]
#![cfg_attr(itest_aarch64, no_main)]
#![deny(missing_docs)]

#[cfg(all(feature = "test-hooks", not(debug_assertions)))]
compile_error!(
    "tairix-test-irq-qemu-aarch64: the `test-hooks` Cargo feature is a \
     debug-only test affordance and must not be enabled in release builds. \
     See AGENTS.md §1 (no hacks) and §5.4.5 (fail closed)."
);

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
