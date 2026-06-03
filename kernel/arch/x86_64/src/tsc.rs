//! Time-Stamp Counter (TSC) suitability validation for x86_64.
//!
//! RustOS uses the x86_64 TSC (`RDTSC`) as its monotonic clock source on
//! this architecture (`apic_timer`, `kernel_arch`). Treating the TSC as a
//! *cross-CPU* monotonic time base is only sound on a part that provides
//! an **Invariant TSC**: a counter that runs at a constant rate
//! regardless of P-/C-/T-state transitions and is synchronised across
//! every logical CPU. On a part without that guarantee the TSC can run at
//! a P-state-dependent rate or drift between cores, so a task migrated by
//! the SMP scheduler could observe time going backwards — which would
//! break the per-CPU monotonic contract `clock_get`/`irq_wait` rely on.
//!
//! The architecture port is the only place allowed to read CPUID
//! (`AGENTS.md` §17.2), so detection lives here. The decoder
//! [`invariant_tsc_supported`](crate::tsc::invariant_tsc_supported) is
//! pure and host-tested; the bare-metal probe
//! [`detect_invariant_tsc`](crate::tsc::detect_invariant_tsc) reads
//! CPUID only on the freestanding
//! target and is consumed by the kernel boot path, which records the
//! result and fails closed before bringing up a second CPU on a part that
//! does not advertise an invariant TSC (`AGENTS.md` §5.4).
//!
//! Reference: Intel SDM Vol. 3B §18.17 ("Invariant TSC"); AMD64 APM
//! Vol. 2 — CPUID leaf `0x8000_0007` EDX bit 8 (`TscInvariant`).

/// CPUID leaf reporting the maximum supported *extended* leaf in `EAX`.
/// The Advanced Power Management leaf is probed only when it is within
/// this bound.
#[cfg_attr(not(all(target_arch = "x86_64", target_os = "none")), allow(dead_code))]
const LEAF_EXT_MAX: u32 = 0x8000_0000;

/// CPUID "Advanced Power Management Information" leaf. `EDX` bit 8 is the
/// Invariant TSC flag on both Intel (SDM Vol. 3B §18.17) and AMD (APM
/// Vol. 2).
#[cfg_attr(not(all(target_arch = "x86_64", target_os = "none")), allow(dead_code))]
const LEAF_ADVANCED_POWER_MGMT: u32 = 0x8000_0007;

/// Bit position of the Invariant TSC flag in leaf `0x8000_0007` `EDX`.
const INVARIANT_TSC_BIT: u32 = 8;

/// Decode the Invariant TSC flag from the raw `EDX` of CPUID leaf
/// `0x8000_0007`.
///
/// Returns `true` iff bit 8 (`TscInvariant`) is set. Pure and
/// host-testable; the bare-metal [`detect_invariant_tsc`] feeds it the
/// register it reads from the CPU.
#[must_use]
pub const fn invariant_tsc_supported(advanced_power_mgmt_edx: u32) -> bool {
    (advanced_power_mgmt_edx >> INVARIANT_TSC_BIT) & 1 == 1
}

/// Returns `true` when the CPU this function executes on advertises an
/// Invariant TSC.
///
/// On the bare-metal target it bounds the maximum extended leaf via leaf
/// `0x8000_0000` before reading leaf `0x8000_0007` (the SDM/APM usage
/// requirement), then decodes the flag with [`invariant_tsc_supported`].
/// A part whose maximum extended leaf does not even reach
/// `0x8000_0007` cannot assert invariance, so the probe returns `false`
/// (fail closed, `AGENTS.md` §5.4.5).
///
/// On the host target there is no bare-metal CPU contract to honour and
/// the production boot validation never runs, so the probe reports
/// `true` to avoid spuriously failing closed in host unit tests of
/// consumers (`AGENTS.md` §1 — no fake hardware in production paths; the
/// real decision is made by [`invariant_tsc_supported`], which *is*
/// host-tested).
#[must_use]
pub fn detect_invariant_tsc() -> bool {
    #[cfg(all(target_arch = "x86_64", target_os = "none"))]
    {
        // `CPUID` is unconditionally available on every x86_64 CPU (it
        // predates the architecture) and is side-effect-free, so these
        // intrinsics are safe on this target.
        let max_ext_leaf = core::arch::x86_64::__cpuid(LEAF_EXT_MAX).eax;
        if max_ext_leaf < LEAF_ADVANCED_POWER_MGMT {
            return false;
        }
        invariant_tsc_supported(core::arch::x86_64::__cpuid(LEAF_ADVANCED_POWER_MGMT).edx)
    }
    #[cfg(not(all(target_arch = "x86_64", target_os = "none")))]
    {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::{invariant_tsc_supported, INVARIANT_TSC_BIT};

    #[test]
    fn invariant_bit_set_is_supported() {
        assert!(invariant_tsc_supported(1 << INVARIANT_TSC_BIT));
        // Other bits set, but not bit 8: not invariant.
        assert!(!invariant_tsc_supported(0xFFFF_FEFF));
        // The whole register clear: not invariant.
        assert!(!invariant_tsc_supported(0));
        // Bit 8 plus unrelated bits: still invariant.
        assert!(invariant_tsc_supported(
            (1 << INVARIANT_TSC_BIT) | 0x0000_00FF
        ));
    }

    #[test]
    fn only_bit_eight_matters() {
        for bit in 0..32u32 {
            let supported = invariant_tsc_supported(1 << bit);
            assert_eq!(
                supported,
                bit == INVARIANT_TSC_BIT,
                "bit {bit} must report invariance iff it is the TscInvariant bit"
            );
        }
    }
}
