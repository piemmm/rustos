//! P11 Chunk B-2 INCREMENT (1) QEMU integration test: a device SPI (the
//! PL031 RTC) wakes an in-kernel service kthread parked on it, under the
//! live eevdf scheduler, on the aarch64 `virt` board.
//!
//! Where `tests/integration/irq_qemu_aarch64` proves the device-IRQ
//! *delivery* path (EL1 vector -> `IrqTable::fire` mask-before-wake)
//! against a hard-coded INTID and a `wfi` poll loop, this vertical proves
//! the two INCREMENT (1) pieces that delivery path exists to serve:
//!
//! 1. **DTB SPI discovery.** It decodes the PL031 RTC node's `interrupts`
//!    triple into its GICv2 INTID through
//!    `tairix_arch_aarch64::fdt::gic_device_intid` over the embedded `virt`
//!    device tree — no board constant.
//! 2. **The kthread-cooperative waiter.** A real in-kernel service kthread,
//!    admitted on the `tairix_kernel_sched_eevdf::Scheduler` via
//!    `tairix_kernel_core::spawn_kthread`, blocks on the bound line through
//!    `tairix_kernel_core::KthreadIrqWaiter` + the shared
//!    `tairix_kernel_irq::block_until_ready` loop, cooperatively yielding
//!    until the RTC SPI fires and wakes it.
//!
//! On boot the test discovers the board (GICv2 bases + timer rate + the RTC
//! SPI) from the embedded tree, builds a kernel-neutral `IrqTable`, binds
//! the RTC line, installs the device-IRQ dispatcher, brings up the EL1
//! vectors + GICv2, routes + arms the RTC SPI, unmasks IRQs, and admits the
//! waiter kthread. The cooperative `step` loop drives the kthread (which
//! parks in `block_until_ready`); when the RTC fires the GIC delivers the
//! SPI to EL1, the dispatcher masks the line and sets the ready flag, and
//! the kthread observes `WaitOutcome::Ready` and exits. The kernel then
//! re-reads the GIC enable bit and asserts the line is masked — the
//! mask-before-wake evidence (`docs/src/security/irq.md`) — before
//! reporting PASS via ARM semihosting. A regression that fails to discover,
//! deliver, wake, or mask never reaches PASS, so the run times out (the
//! documented fail-loud behaviour).

#![cfg_attr(itest_aarch64, no_std)]
#![cfg_attr(itest_aarch64, no_main)]
#![deny(missing_docs)]

#[cfg(all(feature = "test-hooks", not(debug_assertions)))]
compile_error!(
    "tairix-test-irq-kthread-qemu-aarch64: the `test-hooks` Cargo feature is a \
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
