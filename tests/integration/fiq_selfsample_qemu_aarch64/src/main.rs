//! WATCHDOG.md B3 QEMU integration test (`plans/WATCHDOG.md`,
//! `plans/OPEN-DEFECTS.md` D13): prove the debug-only **non-maskable FIQ
//! watchdog self-sample** can observe a core wedged in a `DAIF.I`-masked
//! kernel section — the exact blind spot the maskable IRQ cadence physically
//! cannot see, and the tool the D13 `stress --cpu N` wedge calls for.
//!
//! The IRQ-cadence buddy detector (the shippable design) samples a wedged core
//! only from *another* CPU, and only its ~1 s-stale `pre_silence` context. When
//! a core spins with `DAIF.I` masked (an `IrqSafeSpinLock`, a fault resolver, a
//! busy-spin inside a task body), no maskable interrupt — its own cadence or a
//! buddy's recovery IPI — can reach it. The FIQ self-sample closes this: FIQ is
//! gated by the *separate* `DAIF.F` bit, which a `watchdog-diagnostics` build
//! leaves clear, so a Group-0 cadence delivered as FIQ fires *through* the
//! `DAIF.I` mask and captures a **live** snapshot of the wedged PC.
//!
//! On boot the test reads the GICv2 bases + generic-timer rate from the
//! embedded `virt` device tree, brings up the EL1 vectors + GICv2, and runs the
//! production `probe_fiq_deliverability` capability probe. On the QEMU `virt`
//! board's single-Security-state GIC (`secure=off`) Group 0 / FIQ reaches
//! non-secure EL1, so the probe reports `Supported` and the watchdog routes its
//! cadence PPI to Group 0 as an FIQ. The test then installs a sampling callback,
//! arms a short cadence, deliberately masks `DAIF.I`, and busy-spins in a
//! `#[inline(never)]` marker function issuing no yield and no syscall. The FIQ
//! fires mid-spin and the callback records the live interrupted PC and its
//! `capture_sample_backtrace`.
//!
//! It PASSes once (a) the probe reported `Supported`, (b) an FIQ self-sample
//! fired while `DAIF.I` was masked (the recorded interrupted `DAIF` proves it),
//! (c) the sample interrupted **kernel** context, and (d) the sampled PC *and*
//! the backtrace top land inside the masked-spin marker function — i.e. the
//! self-sample named the section it was stuck in, `sampled=live`. Any shortfall
//! writes a distinct semihosting failure code or the harness times out
//! (fail-loud).

#![cfg_attr(itest_aarch64, no_std)]
#![cfg_attr(itest_aarch64, no_main)]
#![deny(missing_docs)]

#[cfg(all(feature = "test-hooks", not(debug_assertions)))]
compile_error!(
    "tairix-test-fiq-selfsample-qemu-aarch64: the `test-hooks` Cargo feature is a \
     debug-only test affordance and must not be enabled in release builds. \
     See AGENTS.md §1 (no hacks) and §5.4 (fail closed)."
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
