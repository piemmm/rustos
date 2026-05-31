//! Heterogeneous (performance + efficiency core) detection for x86_64.
//!
//! Some recent x86_64 client parts mix high-throughput performance
//! cores with low-power efficiency cores. The kernel needs each logical
//! CPU's `CoreClass` so the scheduler can keep background work on
//! efficiency cores and migrate work that needs throughput onto a
//! performance core (`docs/src/architecture/scheduler.md`).
//!
//! Detection is the architecture port's job (`AGENTS.md` §17.2 / §18.2).
//! The two x86_64 vendors expose the per-core class through different,
//! incompatible CPUID surfaces, so the probe first reads the vendor
//! string from CPUID leaf 0 and then dispatches:
//!
//! - **Intel** — the core type is read from per-core CPUID **leaf 0x1A**
//!   (Hybrid Information Enumeration, Intel SDM Vol. 2A).
//! - **AMD** — there is no leaf-0x1A equivalent. The core class comes
//!   from the Extended CPU Topology **leaf 0x80000026** (AMD64
//!   Architecture Programming Manual Vol. 2), which on heterogeneous
//!   parts reports a per-core power/efficiency ranking.
//!
//! Both decoders (`classify_core_type`, `classify_amd_core`) are pure
//! and host-testable; the CPUID reads only execute the instruction on
//! the bare-metal target, returning the homogeneous default on the host
//! so the crate builds and tests on `x86_64-linux` (`AGENTS.md` §1 — no
//! fake hardware in production paths).
//!
//! Both decoders fail conservative: any value that is not an encoding
//! the vendor has actually published is treated as
//! [`rustos_arch_api::CoreClass::Performance`], the safe homogeneous
//! default the Arch HAL mandates for
//! [`rustos_arch_api::SchedulerArch::core_class`]. They never guess a
//! class from family/model heuristics or frequency tables.

use rustos_arch_api::CoreClass;

/// CPUID leaf that enumerates an Intel core's hybrid type.
///
/// Only read on the bare-metal target; the host build returns the
/// homogeneous default without consulting CPUID.
#[cfg_attr(not(all(target_arch = "x86_64", target_os = "none")), allow(dead_code))]
const LEAF_HYBRID_INFO: u32 = 0x1A;

/// CPUID core-type encoding for an Intel Atom (efficiency) core, found in
/// bits 31:24 of leaf 0x1A `EAX` (Intel SDM Vol. 2A, "CPUID — Leaf 1AH").
const CORE_TYPE_ATOM: u32 = 0x20;

/// CPUID leaf 0 register values for the AMD vendor string `"AuthenticAMD"`.
///
/// Leaf 0 returns the 12-byte vendor string in `EBX`, `EDX`, `ECX` (in
/// that order), each four ASCII bytes packed little-endian.
const AMD_VENDOR_EBX: u32 = u32::from_le_bytes(*b"Auth");
const AMD_VENDOR_EDX: u32 = u32::from_le_bytes(*b"enti");
const AMD_VENDOR_ECX: u32 = u32::from_le_bytes(*b"cAMD");

/// CPUID leaf bounding the maximum supported *extended* leaf, returned in
/// `EAX`. The AMD topology leaf is probed only when it is within bounds.
#[cfg_attr(not(all(target_arch = "x86_64", target_os = "none")), allow(dead_code))]
const LEAF_EXT_MAX: u32 = 0x8000_0000;

/// AMD Extended CPU Topology leaf (AMD64 APM Vol. 2). Sub-leaves
/// enumerate the topology hierarchy; the per-core fields are valid only
/// at the Core level.
#[cfg_attr(not(all(target_arch = "x86_64", target_os = "none")), allow(dead_code))]
const LEAF_EXT_TOPOLOGY: u32 = 0x8000_0026;

/// `LevelType` value (CPUID leaf 0x80000026 `ECX[15:8]`) identifying the
/// Core level, the only level at which the per-core power/efficiency
/// ranking is defined.
const LEVEL_TYPE_CORE: u32 = 0x1;

/// Power/efficiency ranking (CPUID leaf 0x80000026 `EBX[23:16]`) of the
/// lowest-power, lowest-performance core tier.
///
/// AMD64 APM Vol. 2 defines this field as a relative ranking in which a
/// lower value indicates comparatively lower power consumption and lower
/// performance. On a two-tier heterogeneous part the efficiency cores
/// occupy ranking `0` and the performance cores a higher ranking. AMD
/// has not published an absolute numeric mapping of the topology
/// `CoreType` field to a named microarchitecture, so the efficiency
/// tier is recognised from this published ranking field rather than from
/// `CoreType`. Anything but the lowest tier is treated as a performance
/// core (`AGENTS.md` §2.9 fail conservative).
const EFFICIENCY_RANKING: u32 = 0x0;

/// Largest topology sub-leaf the AMD probe will read before giving up.
///
/// The hierarchy enumerated by leaf 0x80000026 (Core, Complex, Die,
/// Socket, …) is short; this bound keeps the bare-metal walk finite even
/// if a part returns a malformed, never-terminating enumeration.
#[cfg_attr(not(all(target_arch = "x86_64", target_os = "none")), allow(dead_code))]
const MAX_TOPOLOGY_SUBLEAF: u32 = 7;

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

/// Returns `true` when the CPUID leaf 0 vendor registers spell
/// `"AuthenticAMD"`.
///
/// `ebx`, `edx`, `ecx` are the leaf-0 registers in vendor-string order.
#[must_use]
pub const fn is_amd_vendor(ebx: u32, edx: u32, ecx: u32) -> bool {
    ebx == AMD_VENDOR_EBX && edx == AMD_VENDOR_EDX && ecx == AMD_VENDOR_ECX
}

/// Decode the [`CoreClass`] from a Core-level sub-leaf of AMD CPUID leaf
/// 0x80000026.
///
/// `flags_eax`, `ranking_ebx`, `level_ecx` are the raw registers of one
/// topology sub-leaf. Per AMD64 APM Vol. 2 the per-core ranking is only
/// valid at the Core level (`ECX[15:8] == 1`) and only when the part
/// advertises both a heterogeneous topology (`EAX[30]`) and an available
/// efficiency ranking (`EAX[29]`). When all three hold, the core is an
/// [`CoreClass::Efficiency`] core iff its power/efficiency ranking
/// (`EBX[23:16]`) is the lowest tier (`EFFICIENCY_RANKING`).
///
/// Every other case — a non-Core level, a part that does not advertise a
/// heterogeneous topology, a part without an available ranking, or any
/// higher ranking tier — is [`CoreClass::Performance`], matching the
/// conservative default the Intel decoder applies to an unknown core
/// type.
#[must_use]
pub const fn classify_amd_core(flags_eax: u32, ranking_ebx: u32, level_ecx: u32) -> CoreClass {
    let level_type = (level_ecx >> 8) & 0xFF;
    if level_type != LEVEL_TYPE_CORE {
        return CoreClass::Performance;
    }
    let heterogeneous = (flags_eax >> 30) & 1 == 1;
    let ranking_available = (flags_eax >> 29) & 1 == 1;
    if !heterogeneous || !ranking_available {
        return CoreClass::Performance;
    }
    let ranking = (ranking_ebx >> 16) & 0xFF;
    if ranking == EFFICIENCY_RANKING {
        CoreClass::Efficiency
    } else {
        CoreClass::Performance
    }
}

/// Returns the [`CoreClass`] of the CPU this function executes on.
///
/// On the bare-metal target it reads the CPUID vendor string from leaf 0
/// and dispatches to the matching vendor probe (AMD leaf 0x80000026,
/// otherwise the Intel leaf 0x1A path). A part that does not advertise a
/// heterogeneous topology is homogeneous and reports
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
        // intrinsics are safe on this target.
        let vendor = core::arch::x86_64::__cpuid(0);
        if is_amd_vendor(vendor.ebx, vendor.edx, vendor.ecx) {
            detect_amd_core_class()
        } else {
            detect_intel_core_class()
        }
    }
    #[cfg(not(all(target_arch = "x86_64", target_os = "none")))]
    {
        CoreClass::Performance
    }
}

/// Intel bare-metal probe: bound the maximum leaf via leaf 0, check the
/// leaf 0x07 `EDX[15]` "Hybrid" feature, then decode leaf 0x1A.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
fn detect_intel_core_class() -> CoreClass {
    // The maximum leaf is bounded via leaf 0 before reading the
    // higher-numbered leaves so an unsupported sub-leaf is never
    // executed, matching the Intel SDM Vol. 2A usage requirement.
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

/// AMD bare-metal probe: bound the maximum extended leaf via leaf
/// 0x80000000, then walk leaf 0x80000026 sub-leaves for the Core level
/// and decode it.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
fn detect_amd_core_class() -> CoreClass {
    // Bound the maximum extended leaf before reading it, mirroring the
    // leaf-0 bound on the Intel path, so an unsupported leaf is never
    // executed (AMD64 APM Vol. 2 usage requirement).
    let max_ext_leaf = core::arch::x86_64::__cpuid(LEAF_EXT_MAX).eax;
    if max_ext_leaf < LEAF_EXT_TOPOLOGY {
        return CoreClass::Performance;
    }
    let mut sub_leaf = 0;
    while sub_leaf <= MAX_TOPOLOGY_SUBLEAF {
        let topo = core::arch::x86_64::__cpuid_count(LEAF_EXT_TOPOLOGY, sub_leaf);
        let level_type = (topo.ecx >> 8) & 0xFF;
        if level_type == 0 {
            // An invalid level type marks the end of the enumeration.
            break;
        }
        if level_type == LEVEL_TYPE_CORE {
            return classify_amd_core(topo.eax, topo.ebx, topo.ecx);
        }
        sub_leaf += 1;
    }
    CoreClass::Performance
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
    fn amd_vendor_string_is_recognised() {
        assert!(is_amd_vendor(
            AMD_VENDOR_EBX,
            AMD_VENDOR_EDX,
            AMD_VENDOR_ECX
        ));
        // The other x86_64 vendor string must not match.
        let intel = (
            u32::from_le_bytes(*b"Genu"),
            u32::from_le_bytes(*b"ineI"),
            u32::from_le_bytes(*b"ntel"),
        );
        assert!(!is_amd_vendor(intel.0, intel.1, intel.2));
        assert!(!is_amd_vendor(0, 0, 0));
    }

    /// Build the Core-level sub-leaf registers for an AMD part that
    /// advertises a heterogeneous topology with an available efficiency
    /// ranking, carrying `ranking` in `EBX[23:16]`.
    fn amd_core_subleaf(ranking: u32) -> (u32, u32, u32) {
        let flags = (1 << 30) | (1 << 29);
        let ranking_field = (ranking & 0xFF) << 16;
        let level = LEVEL_TYPE_CORE << 8;
        (flags, ranking_field, level)
    }

    #[test]
    fn amd_lowest_ranking_core_is_efficiency() {
        // The lowest power/efficiency ranking tier (e.g. a density-
        // optimised core) classifies as an efficiency core.
        let (flags, ranking_field, level) = amd_core_subleaf(EFFICIENCY_RANKING);
        assert_eq!(
            classify_amd_core(flags, ranking_field, level),
            CoreClass::Efficiency
        );
    }

    #[test]
    fn amd_higher_ranking_core_is_performance() {
        // A higher ranking tier (a throughput-optimised core) classifies
        // as a performance core.
        let (flags, ranking_field, level) = amd_core_subleaf(1);
        assert_eq!(
            classify_amd_core(flags, ranking_field, level),
            CoreClass::Performance
        );
        let (flags, ranking_field, level) = amd_core_subleaf(0xFF);
        assert_eq!(
            classify_amd_core(flags, ranking_field, level),
            CoreClass::Performance
        );
    }

    #[test]
    fn amd_non_heterogeneous_or_no_ranking_is_performance() {
        // Heterogeneous bit clear: a homogeneous part, even at the Core
        // level with a zero ranking, is a performance core.
        let level = LEVEL_TYPE_CORE << 8;
        assert_eq!(
            classify_amd_core(1 << 29, 0, level),
            CoreClass::Performance,
            "ranking available but topology not heterogeneous"
        );
        assert_eq!(
            classify_amd_core(1 << 30, 0, level),
            CoreClass::Performance,
            "heterogeneous but ranking not available"
        );
    }

    #[test]
    fn amd_non_core_level_is_performance() {
        // The ranking is only valid at the Core level; a non-Core
        // sub-leaf is treated as the homogeneous default.
        let flags = (1 << 30) | (1 << 29);
        let non_core_level = 0x2 << 8;
        assert_eq!(
            classify_amd_core(flags, 0, non_core_level),
            CoreClass::Performance
        );
    }

    #[test]
    fn amd_unknown_topology_is_performance() {
        // Reserved/zero registers must never misclassify.
        assert_eq!(classify_amd_core(0, 0, 0), CoreClass::Performance);
        assert_eq!(
            classify_amd_core(0xFFFF_FFFF, 0xFFFF_FFFF, 0xFFFF_FFFF),
            CoreClass::Performance,
            "all-ones reports a non-zero ranking => performance"
        );
    }

    #[test]
    fn host_detection_reports_homogeneous_default() {
        // The host build has no heterogeneous topology to model.
        assert_eq!(detect_current_core_class(), CoreClass::Performance);
    }
}
