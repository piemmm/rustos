//! Intel "hybrid" (performance + efficiency core) detection for x86_64.
//!
//! Recent Intel client parts (Alder Lake onward) pair high-throughput
//! "Core" P-cores with low-power "Atom" E-cores. The kernel needs each
//! logical CPU's `CoreClass` so the scheduler can keep background work
//! on efficiency cores and migrate work that needs throughput onto a
//! performance core (`docs/src/architecture/scheduler.md`).
//!
//! Detection is the architecture port's job (`AGENTS.md` §17.2 / §18.2):
//! the class is read from the per-core CPUID **leaf 0x1A** (Hybrid
//! Information Enumeration, Intel SDM Vol. 2A). The result is a static
//! identity of the executing core, so each CPU records its own class as
//! it comes online (`X86_64Arch::record_core_class`).
//!
//! The decoding (`classify_core_type`) is pure and host-testable; the
//! CPUID read (`detect_current_core_class`) only executes the
//! instruction on the bare-metal target, returning the homogeneous
//! default on the host so the crate builds and tests on `x86_64-linux`
//! (`AGENTS.md` §1 — no fake hardware in production paths).

use rustos_arch_api::CoreClass;

/// CPUID leaf that enumerates a core's hybrid type.
///
/// Only read on the bare-metal target; the host build returns the
/// homogeneous default without consulting CPUID.
#[cfg_attr(not(all(target_arch = "x86_64", target_os = "none")), allow(dead_code))]
const LEAF_HYBRID_INFO: u32 = 0x1A;

/// CPUID core-type encoding for an Intel Atom (efficiency) core, found in
/// bits 31:24 of leaf 0x1A `EAX` (Intel SDM Vol. 2A, "CPUID — Leaf 1AH").
const CORE_TYPE_ATOM: u32 = 0x20;

/// Decode the [`CoreClass`] from the raw `EAX` of CPUID leaf 0x1A.
///
/// Bits 31:24 hold the core type. `0x20` is an Intel "Atom" (efficiency)
/// core. `0x40` is an Intel "Core" (performance) core, and any other
/// value — including `0`, which leaf 0x1A reports on a non-hybrid part —
/// is also treated as a performance core, the safe homogeneous default
/// the Arch HAL mandates for
/// [`rustos_arch_api::SchedulerArch::core_class`].
#[must_use]
pub const fn classify_core_type(leaf_1a_eax: u32) -> CoreClass {
    match leaf_1a_eax >> 24 {
        CORE_TYPE_ATOM => CoreClass::Efficiency,
        _ => CoreClass::Performance,
    }
}

/// Returns the [`CoreClass`] of the CPU this function executes on.
///
/// On the bare-metal target it reads CPUID: leaf 0 bounds the maximum
/// supported leaf, leaf 0x07 (`ECX=0`) `EDX[15]` reports the "Hybrid"
/// feature, and only then is leaf 0x1A consulted. A part that does not
/// advertise the hybrid feature is homogeneous and reports
/// [`CoreClass::Performance`].
///
/// On the host target there is no meaningful CPU topology to expose to a
/// unit test, so the homogeneous default is returned.
#[must_use]
pub fn detect_current_core_class() -> CoreClass {
    #[cfg(all(target_arch = "x86_64", target_os = "none"))]
    {
        // `CPUID` is unconditionally available on every x86_64 CPU (it
        // predates the architecture) and is side-effect-free, so these
        // intrinsics are safe on this target. We bound the maximum leaf
        // via leaf 0 before reading the higher-numbered leaves so we
        // never execute an unsupported sub-leaf, matching the Intel SDM
        // Vol. 2A usage requirement.
        let max_leaf = core::arch::x86_64::__cpuid(0).eax;
        if max_leaf < LEAF_HYBRID_INFO {
            return CoreClass::Performance;
        }
        let features = core::arch::x86_64::__cpuid_count(0x07, 0);
        let hybrid = (features.edx >> 15) & 1 == 1;
        if !hybrid {
            return CoreClass::Performance;
        }
        classify_core_type(core::arch::x86_64::__cpuid_count(LEAF_HYBRID_INFO, 0).eax)
    }
    #[cfg(not(all(target_arch = "x86_64", target_os = "none")))]
    {
        CoreClass::Performance
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_type_byte_decodes_to_class() {
        // Bits 31:24 carry the type; the low bits (a per-type model id)
        // are ignored.
        assert_eq!(
            classify_core_type(0x4000_0001),
            CoreClass::Performance,
            "0x40 = Intel Core => performance"
        );
        assert_eq!(
            classify_core_type(0x2000_00FF),
            CoreClass::Efficiency,
            "0x20 = Intel Atom => efficiency"
        );
    }

    #[test]
    fn unknown_core_type_is_treated_as_performance() {
        // 0 is what a non-hybrid part reports; anything unrecognised must
        // fall back to the safe homogeneous default, never misclassify.
        assert_eq!(classify_core_type(0x0000_0000), CoreClass::Performance);
        assert_eq!(classify_core_type(0x7F00_0000), CoreClass::Performance);
    }

    #[test]
    fn host_detection_reports_homogeneous_default() {
        // The host build has no hybrid topology to model.
        assert_eq!(detect_current_core_class(), CoreClass::Performance);
    }
}
