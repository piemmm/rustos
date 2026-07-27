//! riscv64 live-core-frequency source (the Arch HAL
//! [`CoreClock`](tairix_arch_api::CoreClock) slice).
//!
//! The `CpuCycleCounter` reference counter is the
//! architectural `time` CSR (`rdtime`) — a fixed-rate time base whose rate is
//! the device-tree `timebase-frequency` and never tracks the core clock. The
//! live core clock instead comes from the `cycle` CSR (`rdcycle`), which
//! counts *core-clock* cycles and so rises and falls with dynamic
//! voltage/frequency scaling. Dividing the two counters' deltas and scaling
//! by the timebase frequency yields the live frequency
//! ([`tairix_arch_api::frequency_hz`]).
//!
//! Both CSRs are read by S-mode only when the M-mode firmware has granted it
//! (`mcounteren.{CY,TM}`); the standard SBI firmware TAIRiX boots under
//! (OpenSBI on the QEMU `virt` board, SiFive boards) enables all counters for
//! S-mode, which is why the port already reads `rdtime` for its monotonic
//! clock. This slice reads `rdcycle` on the same basis and declares itself
//! [`tairix_arch_api::CoreClockSupport::Supported`] on the freestanding
//! target; the host build has no CSRs and declares itself unsupported,
//! reading zero (no fake hardware in a production path).

use core::sync::atomic::{AtomicU64, Ordering};

use tairix_arch_api::{CoreClock, CoreClockSupport};

/// The `time`-CSR (reference) frequency in Hz, published once at boot from
/// the device-tree `timebase-frequency` the `RiscvArch`
/// handle carries (there is no CSR that reports it). `0` until published — the
/// estimator treats that as "cannot measure" and reports no frequency.
static REFERENCE_HZ: AtomicU64 = AtomicU64::new(0);

/// Publish the `time`-CSR frequency the core-clock ratio scales against.
///
/// Called once at boot from the port's `KernelArch::core_clock` accessor with
/// the discovered `timebase-frequency`. Set-once in spirit (idempotent for
/// the same value); a later differing value would only change the reported
/// scale, never fault.
pub fn set_reference_hz(hz: u64) {
    REFERENCE_HZ.store(hz, Ordering::Relaxed);
}

/// Read the `cycle` CSR (`rdcycle`) — the core-clock cycle count. Reads `0`
/// off the freestanding target (the host has no CSR).
#[inline]
#[must_use]
fn read_cycle() -> u64 {
    #[cfg(all(target_arch = "riscv64", target_os = "none"))]
    {
        let cyc: u64;
        // SAFETY: `rdcycle` reads the unprivileged `cycle` CSR; the SBI
        // firmware enables S-mode access (`mcounteren.CY`) on the boards
        // TAIRiX targets, exactly as for the `time` CSR the monotonic clock
        // already reads. It has no memory side effect.
        unsafe {
            core::arch::asm!("rdcycle {}", out(reg) cyc, options(nomem, nostack, preserves_flags));
        }
        cyc
    }
    #[cfg(not(all(target_arch = "riscv64", target_os = "none")))]
    {
        0
    }
}

/// riscv64 implementation of the Arch HAL core-clock surface: `rdcycle` over
/// the `rdtime`/`timebase-frequency` reference.
///
/// Zero-sized: the counter state is in CSRs and the reference frequency is a
/// module static published at boot.
#[derive(Debug, Default, Clone, Copy)]
pub struct CoreClockCounter;

impl CoreClockCounter {
    /// Construct the riscv64 core-clock handle.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// `true` on the freestanding target, where the CSRs exist.
    #[must_use]
    const fn on_hardware() -> bool {
        cfg!(all(target_arch = "riscv64", target_os = "none"))
    }
}

impl CoreClock for CoreClockCounter {
    fn enable(&self) {
        // Nothing to enable in S-mode: the `cycle`/`time` CSRs are freerunning
        // and their S-mode access is granted by M-mode firmware
        // (`mcounteren`), not something this level can or should toggle.
    }

    fn core_cycles(&self) -> u64 {
        read_cycle()
    }

    fn reference_cycles(&self) -> u64 {
        #[cfg(all(target_arch = "riscv64", target_os = "none"))]
        {
            crate::kernel_arch::read_time()
        }
        #[cfg(not(all(target_arch = "riscv64", target_os = "none")))]
        {
            0
        }
    }

    fn reference_hz(&self) -> u64 {
        if Self::on_hardware() {
            REFERENCE_HZ.load(Ordering::Relaxed)
        } else {
            0
        }
    }

    fn support(&self) -> CoreClockSupport {
        if Self::on_hardware() {
            CoreClockSupport::Supported
        } else {
            CoreClockSupport::Unsupported("host build has no riscv64 cycle/time CSRs")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tairix_arch_api::coreclock;

    #[test]
    fn host_build_is_unsupported_and_reads_zero() {
        // Off the freestanding target the CSRs are absent, so the handle
        // fails closed: unsupported, zero counters, zero reference.
        let clock = CoreClockCounter::new();
        assert!(!clock.support().is_supported());
        assert_eq!(clock.core_cycles(), 0);
        assert_eq!(clock.reference_cycles(), 0);
        assert_eq!(clock.reference_hz(), 0);
        coreclock::conformance::run_all(&clock);
    }
}
