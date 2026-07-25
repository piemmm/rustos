//! aarch64 CPU feature detection and cycle counter.
//!
//! Implements the Arch HAL
//! [`CpuFeatures`](tairix_arch_api::CpuFeatures) and
//! [`CpuCycles`](tairix_arch_api::CpuCycles) surfaces for aarch64.
//!
//! The ISA extensions are read from the architected feature-ID
//! registers (Arm ARM DDI 0487): `ID_AA64ISAR0_EL1` carries the
//! AES/PMULL, SHA1/SHA2/SHA3, CRC32 and LSE-atomics fields, and
//! `ID_AA64PFR0_EL1` carries the Advanced SIMD and DIT fields. Both are
//! readable at EL1 with no architectural side effect.
//!
//! The cycle counter is the virtual count `CNTVCT_EL0` of the
//! architected generic timer — a fixed-rate, monotonically-increasing
//! counter always readable at EL1. It is deliberately preferred over the
//! PMU cycle counter `PMCCNTR_EL0`, which would require enabling the PMU
//! (`PMUSERENR_EL0`/`PMCR_EL0`) and is not guaranteed present or usable
//! under every EL configuration; `CNTVCT_EL0` needs no such boot wiring
//! and is a reliable constant-rate time base for the bounded one-shot
//! benchmark the `lib/cpuops` harness runs (it measures deltas, so the
//! fixed frequency is a wall-time proxy, not a cycle count — sufficient
//! to rank two equally-correct routines).
//!
//! Only the architecture port reads these system registers, so detection
//! lives here. The decoder (`features_from_id_regs`) is pure and
//! host-tested; the register reads execute only on the freestanding
//! target and the host build reports the empty set / an unknown core (no
//! fake hardware in production paths).

use tairix_arch_api::{
    CoreType, CpuCycles, CpuFeature, CpuFeatureSet, CpuFeatures, CpuId, FeatureProfile,
    FeatureSupport,
};

/// Extract the 4-bit feature field at bit offset `shift` from a raw
/// feature-ID register value.
const fn field(reg: u64, shift: u32) -> u64 {
    (reg >> shift) & 0xF
}

// --- ID_AA64ISAR0_EL1 field offsets (Arm ARM DDI 0487) ---
const ISAR0_AES: u32 = 4;
const ISAR0_SHA1: u32 = 8;
const ISAR0_SHA2: u32 = 12;
const ISAR0_CRC32: u32 = 16;
const ISAR0_ATOMIC: u32 = 20;
const ISAR0_SHA3: u32 = 32;

// --- ID_AA64PFR0_EL1 field offsets ---
const PFR0_ADVSIMD: u32 = 20;
const PFR0_DIT: u32 = 48;

/// The `AdvSIMD` field value that means "not implemented" (all other
/// values indicate a present, increasingly-capable Advanced SIMD unit).
const ADVSIMD_ABSENT: u64 = 0xF;

/// `DCZID_EL0.DZP` — bit 4. When set, the `DC ZVA` block-zero instruction is
/// *prohibited* at the current exception level; when clear it is permitted.
const DCZID_DZP: u32 = 4;

/// `true` if `DC ZVA` is usable at the current EL, decoded from a raw
/// `DCZID_EL0` value: the prohibit bit (`DZP`) must be clear.
///
/// Pure and host-testable; the register read lives in [`CpuFeatureDetect::detect`].
/// Failing closed on a prohibited instruction is essential — issuing `DC ZVA`
/// when `DZP == 1` would trap.
#[must_use]
pub const fn dczva_usable(dczid: u64) -> bool {
    (dczid >> DCZID_DZP) & 1 == 0
}

/// Decode the two aarch64 feature-ID registers into a [`CpuFeatureSet`].
///
/// Pure and host-testable: the bare-metal probe feeds it the registers
/// it read. Each field is interpreted per the Arm ARM: a field of `0`
/// means the feature is absent (except `AdvSIMD`, where `0xF` is the
/// absent encoding), so masking a field to `0` removes exactly its bit.
#[must_use]
pub fn features_from_id_regs(isar0: u64, pfr0: u64) -> CpuFeatureSet {
    let mut set = CpuFeatureSet::EMPTY;

    // AES field: 1 => AES, 2 => AES + PMULL/PMULL2.
    let aes = field(isar0, ISAR0_AES);
    if aes >= 1 {
        set = set.with(CpuFeature::Aes);
    }
    if aes >= 2 {
        set = set.with(CpuFeature::Pmull);
    }

    if field(isar0, ISAR0_SHA1) >= 1 {
        set = set.with(CpuFeature::Sha1);
    }
    if field(isar0, ISAR0_SHA2) >= 1 {
        set = set.with(CpuFeature::Sha2);
    }
    if field(isar0, ISAR0_CRC32) >= 1 {
        set = set.with(CpuFeature::Crc32);
    }
    // Atomic (LSE) is advertised as value 2 (LSE) or 3 (LSE + CAS on
    // 128-bit); any value >= 2 implies the base LSE instructions.
    if field(isar0, ISAR0_ATOMIC) >= 2 {
        set = set.with(CpuFeature::Lse);
    }
    if field(isar0, ISAR0_SHA3) >= 1 {
        set = set.with(CpuFeature::Sha3);
    }

    if field(pfr0, PFR0_ADVSIMD) != ADVSIMD_ABSENT {
        set = set.with(CpuFeature::Asimd);
    }
    if field(pfr0, PFR0_DIT) >= 1 {
        set = set.with(CpuFeature::Dit);
    }

    set
}

/// aarch64 implementation of the Arch HAL CPU-feature surface.
///
/// Zero-sized: the detection state lives in the CPU's feature-ID
/// registers, not in the handle.
#[derive(Debug, Default, Clone, Copy)]
pub struct CpuFeatureDetect;

impl CpuFeatureDetect {
    /// Construct the aarch64 CPU-feature handle.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// The honest declaration for aarch64: both detection sources are
    /// architected system registers readable at EL1.
    #[must_use]
    pub const fn declared_profile() -> FeatureProfile {
        FeatureProfile {
            isa_features: FeatureSupport::Supported,
            core_identity: FeatureSupport::Supported,
        }
    }
}

impl CpuFeatures for CpuFeatureDetect {
    fn detect(&self, _cpu: CpuId) -> CpuFeatureSet {
        #[cfg(all(target_arch = "aarch64", target_os = "none"))]
        {
            let isar0: u64;
            let pfr0: u64;
            // SAFETY: both feature-ID registers are readable at EL1 (the
            // boot path drops any EL2 entry to EL1 before the kernel
            // runs) and the reads have no architectural side effect.
            unsafe {
                core::arch::asm!(
                    "mrs {isar0}, id_aa64isar0_el1",
                    "mrs {pfr0}, id_aa64pfr0_el1",
                    isar0 = out(reg) isar0,
                    pfr0 = out(reg) pfr0,
                    options(nomem, nostack, preserves_flags),
                );
            }
            let dczid: u64;
            // SAFETY: `DCZID_EL0` is readable at EL1 with no architectural
            // side effect; it reports the `DC ZVA` block size and whether the
            // instruction is permitted here.
            unsafe {
                core::arch::asm!(
                    "mrs {dczid}, dczid_el0",
                    dczid = out(reg) dczid,
                    options(nomem, nostack, preserves_flags),
                );
            }
            let mut set = features_from_id_regs(isar0, pfr0);
            if dczva_usable(dczid) {
                set = set.with(CpuFeature::DcZva);
            }
            set
        }
        #[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
        {
            CpuFeatureSet::EMPTY
        }
    }

    fn core_type(&self, _cpu: CpuId) -> CoreType {
        #[cfg(all(target_arch = "aarch64", target_os = "none"))]
        {
            let midr: u64;
            // SAFETY: `MIDR_EL1` is readable at EL1 with no side effect.
            unsafe {
                core::arch::asm!(
                    "mrs {midr}, midr_el1",
                    midr = out(reg) midr,
                    options(nomem, nostack, preserves_flags),
                );
            }
            CoreType {
                model: crate::cpuname::name_for_midr(midr),
                // The static big.LITTLE class is discovered from the
                // device tree by the scheduler handle; the MIDR already
                // distinguishes big from LITTLE cores (different part
                // numbers), which is what ops-table keying needs, so this
                // reports the safe homogeneous default.
                class: tairix_arch_api::CoreClass::Performance,
                raw_id: midr,
            }
        }
        #[cfg(not(all(target_arch = "aarch64", target_os = "none")))]
        {
            CoreType::UNKNOWN
        }
    }

    fn profile(&self) -> FeatureProfile {
        Self::declared_profile()
    }
}

/// aarch64 implementation of the Arch HAL cycle-counter surface (the
/// generic-timer virtual count `CNTVCT_EL0`).
#[derive(Debug, Default, Clone, Copy)]
pub struct CpuCycleCounter;

impl CpuCycleCounter {
    /// Construct the aarch64 cycle-counter handle.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl CpuCycles for CpuCycleCounter {
    fn cpu_cycles(&self) -> u64 {
        #[cfg(all(target_arch = "aarch64", target_os = "none"))]
        {
            let cnt: u64;
            // SAFETY: `CNTVCT_EL0` is the architected generic-timer
            // virtual count, readable at EL1 with no side effect and
            // monotonically increasing.
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

    fn cycles_monotonic_hint(&self) -> bool {
        // `CNTVCT_EL0` is the architected, fixed-frequency generic-timer
        // count: a reliable constant-rate time base by definition.
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tairix_arch_api::{cpucycles, cpufeatures};

    #[test]
    fn no_fields_decode_to_the_empty_set() {
        // Truly empty requires the AdvSIMD field set to its "absent"
        // encoding (0xF); a zero PFR0 means AdvSIMD *present*.
        let empty = features_from_id_regs(0, ADVSIMD_ABSENT << PFR0_ADVSIMD);
        assert_eq!(empty, CpuFeatureSet::EMPTY);
        // A zero PFR0 field 0 means AdvSIMD present, so ASIMD is set even
        // with an all-zero ISAR0; assert that distinction explicitly.
        assert!(features_from_id_regs(0, 0).contains(CpuFeature::Asimd));
    }

    #[test]
    fn aes_field_gates_aes_then_pmull() {
        // AES = 1: AES present, PMULL absent.
        let set = features_from_id_regs(1 << ISAR0_AES, ADVSIMD_ABSENT << PFR0_ADVSIMD);
        assert!(set.contains(CpuFeature::Aes));
        assert!(!set.contains(CpuFeature::Pmull));
        // AES = 2: both AES and PMULL.
        let set = features_from_id_regs(2 << ISAR0_AES, ADVSIMD_ABSENT << PFR0_ADVSIMD);
        assert!(set.contains(CpuFeature::Aes));
        assert!(set.contains(CpuFeature::Pmull));
    }

    #[test]
    fn crc32_sha_and_lse_decode() {
        let isar0 = (1 << ISAR0_CRC32)
            | (1 << ISAR0_SHA1)
            | (1 << ISAR0_SHA2)
            | (1 << ISAR0_SHA3)
            | (2 << ISAR0_ATOMIC);
        let set = features_from_id_regs(isar0, ADVSIMD_ABSENT << PFR0_ADVSIMD);
        assert!(set.contains(CpuFeature::Crc32));
        assert!(set.contains(CpuFeature::Sha1));
        assert!(set.contains(CpuFeature::Sha2));
        assert!(set.contains(CpuFeature::Sha3));
        assert!(set.contains(CpuFeature::Lse));
        // LSE requires field >= 2; a value of 1 does not set it.
        let set = features_from_id_regs(1 << ISAR0_ATOMIC, ADVSIMD_ABSENT << PFR0_ADVSIMD);
        assert!(!set.contains(CpuFeature::Lse));
    }

    #[test]
    fn advsimd_absent_encoding_clears_the_bit() {
        // AdvSIMD field 0xF => not implemented.
        let set = features_from_id_regs(0, ADVSIMD_ABSENT << PFR0_ADVSIMD);
        assert!(!set.contains(CpuFeature::Asimd));
        // Any other value (0, 1) => present.
        assert!(features_from_id_regs(0, 0).contains(CpuFeature::Asimd));
        assert!(features_from_id_regs(0, 1 << PFR0_ADVSIMD).contains(CpuFeature::Asimd));
    }

    #[test]
    fn dczva_usable_decodes_the_prohibit_bit() {
        // DZP clear (bit 4 == 0): DC ZVA permitted, whatever the block size.
        assert!(dczva_usable(0));
        assert!(dczva_usable(0x4)); // BS = 4 words, DZP clear.
                                    // DZP set (bit 4 == 1): prohibited — fail closed.
        assert!(!dczva_usable(1 << DCZID_DZP));
        assert!(!dczva_usable((1 << DCZID_DZP) | 0x4));
    }

    #[test]
    fn dit_field_decodes() {
        let set = features_from_id_regs(0, (1 << PFR0_DIT) | (ADVSIMD_ABSENT << PFR0_ADVSIMD));
        assert!(set.contains(CpuFeature::Dit));
    }

    #[test]
    fn cortex_a72_class_part_decodes() {
        // The Raspberry Pi 4 Cortex-A72 advertises AES(+PMULL), SHA1,
        // SHA2, CRC32 and NEON, but no LSE (an ARMv8.1 feature) and no
        // SHA3. Synthesise its ISAR0/PFR0.
        let isar0 = (2 << ISAR0_AES) | (1 << ISAR0_SHA1) | (1 << ISAR0_SHA2) | (1 << ISAR0_CRC32);
        let pfr0 = 0; // AdvSIMD present.
        let set = features_from_id_regs(isar0, pfr0);
        for f in [
            CpuFeature::Aes,
            CpuFeature::Pmull,
            CpuFeature::Sha1,
            CpuFeature::Sha2,
            CpuFeature::Crc32,
            CpuFeature::Asimd,
        ] {
            assert!(set.contains(f), "expected {f:?}");
        }
        assert!(!set.contains(CpuFeature::Lse));
        assert!(!set.contains(CpuFeature::Sha3));
    }

    #[test]
    fn declared_profile_is_honest_and_release_ready() {
        let profile = CpuFeatureDetect::new().profile();
        assert_eq!(profile.validate(), Ok(()));
        assert!(profile.is_release_ready());
    }

    #[test]
    fn passes_cpufeatures_conformance() {
        cpufeatures::conformance::run_all(&CpuFeatureDetect::new());
        let dynamic: &dyn CpuFeatures = &CpuFeatureDetect::new();
        cpufeatures::conformance::run_all(dynamic);
    }

    #[test]
    fn passes_cpucycles_conformance() {
        cpucycles::conformance::run_all(&CpuCycleCounter::new());
        assert!(CpuCycleCounter::new().cycles_monotonic_hint());
    }
}
