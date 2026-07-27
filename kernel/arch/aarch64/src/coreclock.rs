//! aarch64 live-core-frequency source (the Arch HAL
//! [`CoreClock`](tairix_arch_api::CoreClock) slice).
//!
//! The `CpuCycleCounter` reference counter is the
//! architected generic-timer virtual count `CNTVCT_EL0` — a fixed-rate time
//! base whose rate is `CNTFRQ_EL0` and never tracks the core clock. The live
//! core clock instead comes from the PMU cycle counter `PMCCNTR_EL0`, which
//! counts *core-clock* cycles and therefore rises and falls with dynamic
//! voltage/frequency scaling. Dividing the two counters' deltas and scaling
//! by `CNTFRQ_EL0` yields the live frequency
//! ([`tairix_arch_api::frequency_hz`]).
//!
//! `PMCCNTR_EL0` requires the PMU to be present and enabled, so this slice:
//!
//! * gates every access on `ID_AA64DFR0_EL1.PMUVer` — a machine with no PMU
//!   declares [`tairix_arch_api::CoreClockSupport::Unsupported`] and its reads
//!   fail closed to `0` (never a trap, never a fabricated rate); and
//! * enables the cycle counter per-CPU in `CoreClock::enable`
//!   (`PMCR_EL0.{E,C,LC}` + `PMCNTENSET_EL0[31]`, counting at every exception
//!   level via a zeroed `PMCCFILTR_EL0`), which the kernel calls on the boot
//!   CPU and on each secondary as it comes up.
//!
//! Only the architecture port touches these system registers; the host build
//! declares the source unsupported and reads zero (no fake hardware in a
//! production path).

use tairix_arch_api::{CoreClock, CoreClockSupport};

/// Read the architected generic-timer virtual count `CNTVCT_EL0` — the
/// fixed-rate reference counter shared by the cycle-counter benchmark handle
/// and this slice's frequency ratio. Reads `0` off the freestanding target.
#[inline]
#[must_use]
pub fn read_cntvct() -> u64 {
    #[cfg(all(target_arch = "aarch64", target_os = "none"))]
    {
        let cnt: u64;
        // SAFETY: `CNTVCT_EL0` is the architected generic-timer virtual
        // count, readable at EL1 with no side effect and monotonically
        // increasing.
        unsafe {
            core::arch::asm!(
                "mrs {cnt}, cntvct_el0",
                cnt = out(reg) cnt,
                options(nomem, nostack, preserves_flags),
            );
        }
        cnt
    }
    #[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
    {
        0
    }
}

/// Read the generic-timer frequency `CNTFRQ_EL0`, in Hz — the reference
/// counter's fixed rate. Reads `0` off the freestanding target.
#[inline]
#[must_use]
fn read_cntfrq() -> u64 {
    #[cfg(all(target_arch = "aarch64", target_os = "none"))]
    {
        let hz: u64;
        // SAFETY: `CNTFRQ_EL0` reports the generic-timer frequency and is
        // readable at EL1 with no side effect.
        unsafe {
            core::arch::asm!(
                "mrs {hz}, cntfrq_el0",
                hz = out(reg) hz,
                options(nomem, nostack, preserves_flags),
            );
        }
        hz
    }
    #[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
    {
        0
    }
}

/// `ID_AA64DFR0_EL1.PMUVer` field offset and the two "no usable PMU"
/// encodings (`0` = not implemented, `0xF` = IMPDEF, not the architected
/// PMU): any other value is a present `PMUv3` whose `PMCCNTR_EL0` we can use.
const PMUVER_SHIFT: u32 = 8;

/// `true` if a `PMUv3` cycle counter is present, decoded from a raw
/// `ID_AA64DFR0_EL1` value. Pure and host-testable.
#[must_use]
pub const fn pmu_present(dfr0: u64) -> bool {
    let pmuver = (dfr0 >> PMUVER_SHIFT) & 0xF;
    pmuver != 0 && pmuver != 0xF
}

/// Read `ID_AA64DFR0_EL1` and decode whether a usable PMU is present. Reads
/// as "absent" off the freestanding target (the host has no PMU).
#[inline]
#[must_use]
fn pmu_available() -> bool {
    #[cfg(all(target_arch = "aarch64", target_os = "none"))]
    {
        let dfr0: u64;
        // SAFETY: `ID_AA64DFR0_EL1` is an architected feature-ID register
        // readable at EL1 with no side effect.
        unsafe {
            core::arch::asm!(
                "mrs {dfr0}, id_aa64dfr0_el1",
                dfr0 = out(reg) dfr0,
                options(nomem, nostack, preserves_flags),
            );
        }
        pmu_present(dfr0)
    }
    #[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
    {
        false
    }
}

/// aarch64 implementation of the Arch HAL core-clock surface: `PMCCNTR_EL0`
/// over the `CNTVCT_EL0`/`CNTFRQ_EL0` reference.
///
/// Zero-sized: the state lives in the CPU's registers, not the handle.
#[derive(Debug, Default, Clone, Copy)]
pub struct CoreClockCounter;

impl CoreClockCounter {
    /// Construct the aarch64 core-clock handle.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl CoreClock for CoreClockCounter {
    fn enable(&self) {
        #[cfg(all(target_arch = "aarch64", target_os = "none"))]
        {
            if !pmu_available() {
                return;
            }
            // SAFETY: the PMU is present (checked above) and we run at EL1,
            // where these PMU registers are accessible (EL2 boot left
            // `MDCR_EL2.TPM` clear). Enabling the counter has no effect on
            // any other subsystem: the kernel reads `PMCCNTR_EL0` only for
            // frequency reporting.
            unsafe {
                let mut pmcr: u64;
                core::arch::asm!(
                    "mrs {pmcr}, pmcr_el0",
                    pmcr = out(reg) pmcr,
                    options(nomem, nostack, preserves_flags),
                );
                // E (bit 0) enable all counters, C (bit 2) reset the cycle
                // counter, LC (bit 6) make it 64-bit so it does not wrap at
                // 32 bits during a sample interval.
                pmcr |= (1 << 0) | (1 << 2) | (1 << 6);
                core::arch::asm!(
                    "msr pmcr_el0, {pmcr}",
                    // PMCCFILTR_EL0 = 0: count core cycles at every exception
                    // level (no EL filtering), so the rate reflects the whole
                    // core, not just user or just kernel time.
                    "msr pmccfiltr_el0, xzr",
                    // PMCNTENSET_EL0 bit 31 enables the dedicated cycle
                    // counter.
                    "msr pmcntenset_el0, {en}",
                    "isb",
                    pmcr = in(reg) pmcr,
                    en = in(reg) 1u64 << 31,
                    options(nomem, nostack, preserves_flags),
                );
            }
        }
    }

    fn core_cycles(&self) -> u64 {
        #[cfg(all(target_arch = "aarch64", target_os = "none"))]
        {
            if !pmu_available() {
                return 0;
            }
            let cyc: u64;
            // SAFETY: the PMU is present (checked above) and `PMCCNTR_EL0`
            // is readable at EL1 with no side effect once enabled.
            unsafe {
                core::arch::asm!(
                    "mrs {cyc}, pmccntr_el0",
                    cyc = out(reg) cyc,
                    options(nomem, nostack, preserves_flags),
                );
            }
            cyc
        }
        #[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
        {
            0
        }
    }

    fn reference_cycles(&self) -> u64 {
        if pmu_available() {
            read_cntvct()
        } else {
            0
        }
    }

    fn reference_hz(&self) -> u64 {
        if pmu_available() {
            read_cntfrq()
        } else {
            0
        }
    }

    fn support(&self) -> CoreClockSupport {
        if pmu_available() {
            CoreClockSupport::Supported
        } else {
            CoreClockSupport::Unsupported(
                "no architected PMUv3 cycle counter (ID_AA64DFR0_EL1.PMUVer) on this core",
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tairix_arch_api::coreclock;

    #[test]
    fn pmuver_field_decodes() {
        // 0 = not implemented, 0xF = IMPDEF non-architected: both unusable.
        assert!(!pmu_present(0));
        assert!(!pmu_present(0xF << PMUVER_SHIFT));
        // Any architected version (e.g. PMUv3p4 == 5) is usable.
        assert!(pmu_present(5 << PMUVER_SHIFT));
        assert!(pmu_present(1 << PMUVER_SHIFT));
    }

    #[test]
    fn host_build_is_unsupported_and_reads_zero() {
        // On the host (not aarch64/none) the PMU is unavailable, so the
        // handle must fail closed: unsupported, zero counters.
        let clock = CoreClockCounter::new();
        assert!(!clock.support().is_supported());
        coreclock::conformance::run_all(&clock);
    }
}
