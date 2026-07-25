//! Arch-neutral CPU-feature vocabulary shared across the workspace.
//!
//! Every TAIRiX image is compiled against a conservative *build-time floor*
//! — the common instruction set of every machine that image must boot
//! (`plans/FIX-HARDWARE-FEATURES.md` P0). Anything the booted CPU offers
//! *above* that floor — CRC32, the aarch64 crypto extension, wide SIMD,
//! hardware AES — is reachable only by asking the silicon at runtime which
//! extensions it actually implements, then dispatching to an
//! extension-using routine only on cores that have it.
//!
//! That runtime answer is a deterministic fact ([`CpuFeatureSet`]) read from
//! CPU-ID registers by the architecture ports, and it is consumed by the
//! generic `lib/cpuops` dispatch framework to pick the fastest *correct*,
//! feature-legal implementation of each accelerated operation. Both the
//! architecture HAL (which produces the set) and `lib/cpuops` (which consumes
//! it) need the *same* definition of "which extension", so — exactly like the
//! hardware tree in [`crate::hwtree`] — that definition lives here in the one
//! dependency-free ABI crate both layers already depend on, rather than in
//! either of them (a `lib/*` crate may not depend on `kernel/*`, and two
//! private copies of the vocabulary would drift).
//!
//! The vocabulary is *capability* only: it answers "does this core have the
//! instruction?", never "which routine is fastest?". Benchmarking to discover
//! whether an instruction exists would be a defect — an absent instruction
//! traps, so the capability gate must be exact and read from ID registers.

/// A single detectable CPU instruction-set extension.
///
/// The enum is closed and cross-architecture: each variant is one extension a
/// consumer gates on, and its discriminant is its stable bit index in a
/// [`CpuFeatureSet`]. A variant is only ever set by the port whose ISA defines
/// it, so a `CpuFeatureSet` produced on aarch64 never has an x86_64-only bit
/// set and vice versa.
///
/// New extensions are appended (never renumbered): the discriminant is a wire
/// position a [`CpuFeatureSet`]'s bits and the pin/log records depend on.
#[repr(u8)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum CpuFeature {
    // --- aarch64 ---
    /// aarch64 CRC32 instructions (`crc32*`), `ID_AA64ISAR0_EL1.CRC32`.
    Crc32 = 0,
    /// aarch64 AES instructions, `ID_AA64ISAR0_EL1.AES` >= 1.
    Aes = 1,
    /// aarch64 polynomial multiply (`PMULL`/`PMULL2`),
    /// `ID_AA64ISAR0_EL1.AES` >= 2.
    Pmull = 2,
    /// aarch64 SHA1 instructions, `ID_AA64ISAR0_EL1.SHA1`.
    Sha1 = 3,
    /// aarch64 SHA2 (SHA-256) instructions, `ID_AA64ISAR0_EL1.SHA2` >= 1.
    Sha2 = 4,
    /// aarch64 SHA3 instructions, `ID_AA64ISAR0_EL1.SHA3`.
    Sha3 = 5,
    /// aarch64 Large System Extensions (atomics), `ID_AA64ISAR0_EL1.Atomic`.
    Lse = 6,
    /// aarch64 Advanced SIMD (NEON), `ID_AA64PFR0_EL1.AdvSIMD` != 0xF.
    /// Baseline-present on ARMv8-A but represented for completeness.
    Asimd = 7,
    /// aarch64 Data-Independent Timing, `ID_AA64PFR0_EL1.DIT`.
    Dit = 8,
    /// aarch64 Data Cache Zero by VA (`DC ZVA`) usable at the current
    /// exception level — `DCZID_EL0.DZP == 0`. `DC ZVA` zeroes a whole
    /// cache-block (`4 << DCZID_EL0.BS` bytes) in one instruction without a
    /// read-for-ownership, the fastest way to clear a page; `DZP == 1` means
    /// the instruction is prohibited here and the bit is absent (fail closed).
    DcZva = 9,

    // --- x86_64 ---
    /// x86_64 SSE2 (baseline on x86-64), CPUID.1:EDX.26.
    Sse2 = 16,
    /// x86_64 SSSE3, CPUID.1:ECX.9.
    Ssse3 = 17,
    /// x86_64 SSE4.2 — carries the `crc32` instruction and `POPCNT`,
    /// CPUID.1:ECX.20.
    Sse42 = 18,
    /// x86_64 AVX, CPUID.1:ECX.28.
    Avx = 19,
    /// x86_64 AVX2, CPUID.7.0:EBX.5.
    Avx2 = 20,
    /// x86_64 AES-NI, CPUID.1:ECX.25.
    AesNi = 21,
    /// x86_64 carry-less multiply (`PCLMULQDQ`), CPUID.1:ECX.1.
    Pclmulqdq = 22,
    /// x86_64 SHA-NI, CPUID.7.0:EBX.29.
    ShaNi = 23,
    /// x86_64 `RDRAND`, CPUID.1:ECX.30.
    Rdrand = 24,
    /// x86_64 `RDSEED`, CPUID.7.0:EBX.18.
    Rdseed = 25,
    /// x86_64 Enhanced `REP MOVSB`/`STOSB` (ERMS), CPUID.7.0:EBX.9. `REP
    /// STOSB` is correct on every x86_64 CPU; with ERMS the microcode uses a
    /// wide, cache-optimised path, making it the fastest general memory fill,
    /// so a fill routine is only worth selecting over the baseline when this
    /// bit is present.
    Erms = 26,

    // --- riscv64 ---
    /// riscv64 `Zbb` basic bit-manipulation extension.
    Zbb = 40,
    /// riscv64 `Zbc` carry-less multiply extension.
    Zbc = 41,
    /// riscv64 `Zbkc` carry-less multiply for cryptography.
    Zbkc = 42,
    /// riscv64 `V` vector extension.
    VectorV = 43,
}

impl CpuFeature {
    /// The stable bit index of this feature in a [`CpuFeatureSet`].
    #[must_use]
    pub const fn bit(self) -> u32 {
        self as u32
    }
}

/// An arch-neutral set of the CPU extensions a core implements.
///
/// A port produces one of these from its ID source; consumers test membership
/// with [`Self::contains`]. It is a plain 64-bit bitset — cheap to copy, cheap
/// to hash, and directly loggable/pinnable — with every bit position fixed by
/// a [`CpuFeature`] discriminant so the encoding is stable across boots and
/// builds.
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default, Hash)]
pub struct CpuFeatureSet(u64);

impl CpuFeatureSet {
    /// The empty set — no extensions present. The honest answer for a port
    /// that cannot read ISA features (e.g. the wasm32 host).
    pub const EMPTY: CpuFeatureSet = CpuFeatureSet(0);

    /// Construct an empty set.
    #[must_use]
    pub const fn new() -> Self {
        Self::EMPTY
    }

    /// `true` if `feature` is present in this set.
    #[must_use]
    pub const fn contains(self, feature: CpuFeature) -> bool {
        (self.0 >> feature.bit()) & 1 == 1
    }

    /// Return a copy of this set with `feature` added — the builder a port
    /// uses while decoding its ID register.
    #[must_use]
    pub const fn with(self, feature: CpuFeature) -> Self {
        Self(self.0 | (1u64 << feature.bit()))
    }

    /// Add `feature` to this set in place.
    pub fn insert(&mut self, feature: CpuFeature) {
        self.0 |= 1u64 << feature.bit();
    }

    /// `true` if this set contains every feature in `required` — the absolute
    /// capability gate a `lib/cpuops` candidate survives (an unsupported
    /// instruction is never reached).
    #[must_use]
    pub fn contains_all(self, required: &[CpuFeature]) -> bool {
        let mut i = 0;
        while i < required.len() {
            if !self.contains(required[i]) {
                return false;
            }
            i += 1;
        }
        true
    }

    /// The raw bits — for the audit-log record and the reproducible-build pin,
    /// never for a consumer's capability test (use [`Self::contains`]).
    #[must_use]
    pub const fn bits(self) -> u64 {
        self.0
    }

    /// Reconstruct a set from raw [`Self::bits`] (the pin/log inverse).
    #[must_use]
    pub const fn from_bits(bits: u64) -> Self {
        Self(bits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feature_bits_are_distinct_and_stable() {
        // No two variants share a bit position.
        let all = [
            CpuFeature::Crc32,
            CpuFeature::Aes,
            CpuFeature::Pmull,
            CpuFeature::Sha1,
            CpuFeature::Sha2,
            CpuFeature::Sha3,
            CpuFeature::Lse,
            CpuFeature::Asimd,
            CpuFeature::Dit,
            CpuFeature::DcZva,
            CpuFeature::Sse2,
            CpuFeature::Ssse3,
            CpuFeature::Sse42,
            CpuFeature::Avx,
            CpuFeature::Avx2,
            CpuFeature::AesNi,
            CpuFeature::Pclmulqdq,
            CpuFeature::ShaNi,
            CpuFeature::Rdrand,
            CpuFeature::Rdseed,
            CpuFeature::Erms,
            CpuFeature::Zbb,
            CpuFeature::Zbc,
            CpuFeature::Zbkc,
            CpuFeature::VectorV,
        ];
        let mut seen = 0u64;
        for f in all {
            assert!(f.bit() < 64, "feature bit must fit in the 64-bit set");
            let mask = 1u64 << f.bit();
            assert_eq!(seen & mask, 0, "feature {f:?} reuses a bit index");
            seen |= mask;
        }
    }

    #[test]
    fn set_membership_and_builder() {
        let set = CpuFeatureSet::new()
            .with(CpuFeature::Crc32)
            .with(CpuFeature::Aes);
        assert!(set.contains(CpuFeature::Crc32));
        assert!(set.contains(CpuFeature::Aes));
        assert!(!set.contains(CpuFeature::Sha2));
        assert!(set.contains_all(&[CpuFeature::Crc32, CpuFeature::Aes]));
        assert!(!set.contains_all(&[CpuFeature::Crc32, CpuFeature::Sha2]));
        // Empty requirement is vacuously satisfied.
        assert!(set.contains_all(&[]));
    }

    #[test]
    fn set_bits_round_trip() {
        let set = CpuFeatureSet::new()
            .with(CpuFeature::Sse42)
            .with(CpuFeature::Avx2);
        assert_eq!(CpuFeatureSet::from_bits(set.bits()), set);
        assert_eq!(CpuFeatureSet::EMPTY.bits(), 0);
    }

    #[test]
    fn insert_mutates_in_place() {
        let mut set = CpuFeatureSet::new();
        set.insert(CpuFeature::Zbb);
        assert!(set.contains(CpuFeature::Zbb));
    }
}
