//! x86_64 live-core-frequency source (the Arch HAL
//! [`CoreClock`](tairix_arch_api::CoreClock) slice).
//!
//! The `CpuCycleCounter` reference counter is `rdtsc`,
//! an Invariant TSC — a fixed-rate time base that advances at the CPU's
//! *base* frequency and never reflects turbo or throttling. The live core
//! clock instead comes from the `IA32_APERF`/`IA32_MPERF` feedback pair: over
//! any interval `APERF` accrues at the *actual* core frequency and `MPERF` at
//! the fixed TSC rate, so `f = ΔAPERF · tsc_hz / ΔMPERF`
//! ([`tairix_arch_api::frequency_hz`]) is the live clock, turbo and all. This
//! is Intel's "effective frequency" interface.
//!
//! The pair is present only when `CPUID.06H:ECX.0` (hardware coordination
//! feedback) is set — many virtual CPUs (QEMU TCG) do not implement it — so
//! this slice gates on that bit and on a calibrated TSC frequency
//! (`preempt::tsc_hz`, the `MPERF`/reference rate). A CPU lacking
//! either declares [`tairix_arch_api::CoreClockSupport::Unsupported`] and its
//! reads fail closed to `0` (never a `#GP` on an absent MSR, never a
//! fabricated rate). The MSRs are privileged, so the whole source is gated to
//! the freestanding target; a host build (even on an x86_64 developer
//! machine) declares itself unsupported and never executes `rdmsr`.

use tairix_arch_api::{CoreClock, CoreClockSupport};

/// `IA32_MPERF` — the reference (maximum-performance) counter, ticking at the
/// TSC rate. Read only on the freestanding target (privileged `rdmsr`).
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
const IA32_MPERF: u32 = 0xE7;
/// `IA32_APERF` — the actual-performance counter, ticking at the live core
/// frequency. Read only on the freestanding target (privileged `rdmsr`).
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
const IA32_APERF: u32 = 0xE8;

/// `true` if the `APERF`/`MPERF` hardware-coordination-feedback pair is
/// present (`CPUID.06H:ECX.0`). Off the freestanding target this is `false`
/// so no privileged MSR is ever read on the host.
#[inline]
#[must_use]
fn aperf_mperf_present() -> bool {
    #[cfg(all(target_arch = "x86_64", target_os = "none"))]
    {
        // `CPUID` is side-effect-free; leaf 6 exists when the max basic leaf
        // (leaf 0 EAX) reaches it. Bit 0 of ECX is the effective-frequency
        // (APERF/MPERF) capability.
        let max_basic = core::arch::x86_64::__cpuid(0).eax;
        if max_basic < 6 {
            return false;
        }
        core::arch::x86_64::__cpuid_count(6, 0).ecx & 1 != 0
    }
    #[cfg(not(all(target_arch = "x86_64", target_os = "none")))]
    {
        false
    }
}

/// Read a model-specific register. Freestanding target only — the caller
/// gates on [`aperf_mperf_present`] so the MSR is known to exist.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
#[inline]
#[must_use]
fn rdmsr(msr: u32) -> u64 {
    let lo: u32;
    let hi: u32;
    // SAFETY: `rdmsr` runs at ring 0 (the kernel's only privilege level) and
    // the caller has confirmed `msr` (APERF/MPERF) exists via
    // `CPUID.06H:ECX.0`, so the read cannot `#GP`. It has no memory side
    // effect.
    unsafe {
        core::arch::asm!(
            "rdmsr",
            in("ecx") msr,
            out("eax") lo,
            out("edx") hi,
            options(nomem, nostack, preserves_flags),
        );
    }
    (u64::from(hi) << 32) | u64::from(lo)
}

/// x86_64 implementation of the Arch HAL core-clock surface: `IA32_APERF`
/// over the `IA32_MPERF`/TSC reference.
///
/// Zero-sized: the state lives in MSRs and the calibrated TSC frequency is
/// held by `super::preempt`.
#[derive(Debug, Default, Clone, Copy)]
pub struct CoreClockCounter;

impl CoreClockCounter {
    /// Construct the x86_64 core-clock handle.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// `true` when a supported live source is available: the feedback pair is
    /// present and the TSC (reference) frequency has been calibrated.
    #[must_use]
    fn available() -> bool {
        aperf_mperf_present() && calibrated_tsc_hz() != 0
    }
}

/// The calibrated TSC (reference) frequency in Hz, or `0` off the
/// freestanding target where no calibration runs. `preempt::tsc_hz` exists
/// only on the bare-metal build (it reads a boot-calibrated static), so this
/// shim keeps the host build compiling and honestly uncalibrated.
#[inline]
#[must_use]
fn calibrated_tsc_hz() -> u64 {
    #[cfg(all(target_arch = "x86_64", target_os = "none"))]
    {
        crate::preempt::tsc_hz()
    }
    #[cfg(not(all(target_arch = "x86_64", target_os = "none")))]
    {
        0
    }
}

impl CoreClock for CoreClockCounter {
    fn enable(&self) {
        // `APERF`/`MPERF` are free-running whenever the CPU implements the
        // feedback pair; there is nothing to enable.
    }

    fn core_cycles(&self) -> u64 {
        #[cfg(all(target_arch = "x86_64", target_os = "none"))]
        {
            if aperf_mperf_present() {
                return rdmsr(IA32_APERF);
            }
        }
        0
    }

    fn reference_cycles(&self) -> u64 {
        #[cfg(all(target_arch = "x86_64", target_os = "none"))]
        {
            if aperf_mperf_present() {
                return rdmsr(IA32_MPERF);
            }
        }
        0
    }

    fn reference_hz(&self) -> u64 {
        if Self::available() {
            calibrated_tsc_hz()
        } else {
            0
        }
    }

    fn support(&self) -> CoreClockSupport {
        if Self::available() {
            CoreClockSupport::Supported
        } else {
            CoreClockSupport::Unsupported(
                "no IA32_APERF/MPERF effective-frequency pair (CPUID.06H:ECX.0) or uncalibrated TSC",
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tairix_arch_api::coreclock;

    #[test]
    fn host_build_is_unsupported_and_reads_zero() {
        // Off the freestanding target the MSRs are unreachable, so the handle
        // fails closed: unsupported, zero counters — no `rdmsr` executes.
        let clock = CoreClockCounter::new();
        assert!(!clock.support().is_supported());
        assert_eq!(clock.core_cycles(), 0);
        assert_eq!(clock.reference_cycles(), 0);
        assert_eq!(clock.reference_hz(), 0);
        coreclock::conformance::run_all(&clock);
    }
}
