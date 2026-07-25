//! CPU feature-detection surface of the Arch HAL.
//!
//! Every TAIRiX image is compiled against a conservative *build-time
//! floor* — the common instruction set of every machine that image must
//! boot (`plans/FIX-HARDWARE-FEATURES.md` P0). Anything the booted CPU
//! offers *above* that floor — CRC32, the aarch64 crypto extension, wide
//! SIMD, hardware AES — is reachable only by asking the silicon at
//! runtime which extensions it actually implements. That question is
//! deterministic (it is read from CPU ID registers, never benchmarked)
//! and target-divergent (the register and its encoding differ per ISA),
//! so it is a closed Arch HAL slice, modelled slot-for-slot on the
//! [`super::memtag`] surface.
//!
//! # Two axes, never conflated
//!
//! *Capability* — "does this core have the instruction?" — is the only
//! thing this slice answers, and it answers it from ID registers.
//! *Performance* — "which equally-correct implementation is fastest
//! here?" — is a separate decision made by `lib/cpuops` over the
//! [`super::cpucycles`] counter. Benchmarking to discover whether an
//! instruction exists would be a defect: an absent instruction traps,
//! so the capability gate must be exact.
//!
//! # What lives here
//!
//! * [`CpuFeature`] — the closed enum naming each detectable extension,
//!   each mapped to a stable bit index.
//! * [`CpuFeatureSet`] — the arch-neutral bitset a port produces from its
//!   ID source; the CPU analogue of the hardware tree. Consumers gate a
//!   candidate on [`CpuFeatureSet::contains`], never a raw mask.
//! * [`CoreType`] — the per-core-type key. Heterogeneous SMP
//!   (`big.LITTLE`, Intel hybrid) means a feature set measured on one
//!   core does not describe another, so `lib/cpuops` keys its ops tables
//!   on this.
//! * [`FeatureProfile`] / [`FeatureSupport`] — the honest declaration,
//!   exactly like [`super::memtag::TaggingProfile`]: a port that cannot
//!   trust a detection source declares it
//!   [`FeatureSupport::Unsupported`] (with a justification) or
//!   [`FeatureSupport::Pending`] rather than fabricating a bit.
//! * [`CpuFeatures`] — the per-port handle the kernel reaches through.
//! * [`conformance`] — the conformance vertical every port runs.

use crate::{CoreClass, CpuId};

/// A single detectable CPU instruction-set extension.
///
/// The enum is closed and cross-architecture: each variant is one
/// extension a consumer gates on, and its discriminant is its stable bit
/// index in a [`CpuFeatureSet`]. A variant is only ever set by the port
/// whose ISA defines it, so a `CpuFeatureSet` produced on aarch64 never
/// has an x86_64-only bit set and vice versa.
///
/// New extensions are appended (never renumbered): the discriminant is a
/// wire position a [`CpuFeatureSet`]'s bits and the pin/log records depend
/// on.
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
/// A port produces one of these from its ID source
/// ([`CpuFeatures::detect`]); consumers test membership with
/// [`Self::contains`]. It is a plain 64-bit bitset — cheap to copy, cheap
/// to hash, and directly loggable/pinnable — with every bit position
/// fixed by a [`CpuFeature`] discriminant so the encoding is stable
/// across boots and builds.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default, Hash)]
pub struct CpuFeatureSet(u64);

impl CpuFeatureSet {
    /// The empty set — no extensions present. The honest answer for a
    /// port that cannot read ISA features (e.g. the wasm32 host).
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

    /// Return a copy of this set with `feature` added — the builder a
    /// port uses while decoding its ID register.
    #[must_use]
    pub const fn with(self, feature: CpuFeature) -> Self {
        Self(self.0 | (1u64 << feature.bit()))
    }

    /// Add `feature` to this set in place.
    pub fn insert(&mut self, feature: CpuFeature) {
        self.0 |= 1u64 << feature.bit();
    }

    /// `true` if this set contains every feature in `required` — the
    /// absolute capability gate a `lib/cpuops` candidate survives
    /// (an unsupported instruction is never reached).
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

    /// The raw bits — for the audit-log record and the reproducible-build
    /// pin, never for a consumer's capability test (use [`Self::contains`]).
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

/// The identity of a distinct core type on a (possibly heterogeneous)
/// machine — the key `lib/cpuops` builds one ops table per.
///
/// On asymmetric silicon (`big.LITTLE`, Intel hybrid) a feature set and a
/// benchmark measured on one cluster do not describe another, so the ops
/// table is resolved per `CoreType` as each CPU comes up, never measured
/// once on the boot CPU and imposed globally.
///
/// [`Self::raw_id`] is the real discriminator (`MIDR_EL1` on aarch64, the
/// `CPUID` signature on x86_64, `mvendorid:marchid:mimpid` on riscv64):
/// distinct microarchitectures carry distinct raw ids — big and LITTLE
/// Arm cores already differ in `MIDR` part number — so keying on it
/// separates clusters without a second discovery pass. [`Self::model`] is
/// the human-readable name for the audit log (`None` when the port cannot
/// name the part honestly), and [`Self::class`] is the static
/// [`CoreClass`] the scheduler already tracks.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct CoreType {
    /// Marketing name of the part, or `None` for an honest "unknown".
    pub model: Option<&'static str>,
    /// The static performance class (defaults to
    /// [`CoreClass::Performance`], the homogeneous answer).
    pub class: CoreClass,
    /// The raw hardware identity register value — the discriminator.
    pub raw_id: u64,
}

impl CoreType {
    /// An unknown core type: no name, performance class, zero id. The
    /// honest answer for a port with no CPU-identity source (wasm32) and
    /// the safe out-of-range answer.
    pub const UNKNOWN: CoreType = CoreType {
        model: None,
        class: CoreClass::Performance,
        raw_id: 0,
    };
}

/// One CPU-feature-detection source's status on a given port.
///
/// Mirrors [`super::memtag::Tagging`]: a port takes exactly one honest
/// position per detection source. [`FeatureSupport::Unsupported`] is
/// permitted only where the port genuinely has no such source (the wasm32
/// host does not expose native ISA extensions), and the payload records
/// why. [`FeatureSupport::Pending`] is for a source the silicon has but a
/// not-yet-landed probe must wire up (e.g. an empirically-probed platform
/// capability such as non-secure FIQ deliverability,
/// `plans/FIX-HARDWARE-FEATURES.md`).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum FeatureSupport {
    /// The port drives this detection source.
    Supported,
    /// The port has no such source. The payload is the justification; it
    /// must be non-empty.
    Unsupported(&'static str),
    /// The source exists but is not wired up yet. The payload is the
    /// tracking note; it must be non-empty.
    Pending(&'static str),
}

impl FeatureSupport {
    /// `true` if this source is [`FeatureSupport::Supported`].
    #[must_use]
    pub const fn is_supported(self) -> bool {
        matches!(self, Self::Supported)
    }

    /// `true` if this source is a tracked [`FeatureSupport::Pending`].
    #[must_use]
    pub const fn is_pending(self) -> bool {
        matches!(self, Self::Pending(_))
    }

    /// `true` if release-ready: supported or a justified `Unsupported`. A
    /// `Pending` source is not release-ready.
    #[must_use]
    pub const fn is_release_ready(self) -> bool {
        matches!(self, Self::Supported | Self::Unsupported(_))
    }

    /// The explanatory note for a non-supported decision, or `None`.
    #[must_use]
    pub const fn detail(self) -> Option<&'static str> {
        match self {
            Self::Supported => None,
            Self::Unsupported(reason) | Self::Pending(reason) => Some(reason),
        }
    }
}

/// A port's honest declaration of the detection sources it drives.
///
/// Two genuinely distinct properties, so two slots:
///
/// * [`Self::isa_features`] — the port reads ISA extension bits from a
///   CPU-ID source (CPUID / `ID_AA64ISAR0_EL1` / `misa`) into a
///   [`CpuFeatureSet`]. When this is not [`FeatureSupport::Supported`],
///   [`CpuFeatures::detect`] returns [`CpuFeatureSet::EMPTY`] — no bit is
///   fabricated.
/// * [`Self::core_identity`] — the port reads a stable per-core identity
///   register into a [`CoreType`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct FeatureProfile {
    /// ISA extension bits are read from a CPU-ID source.
    pub isa_features: FeatureSupport,
    /// A stable per-core identity is read from a hardware register.
    pub core_identity: FeatureSupport,
}

/// A single named slot of a [`FeatureProfile`], yielded by
/// [`FeatureProfile::entries`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct FeatureEntry {
    /// Stable, human-readable name of the slot.
    pub name: &'static str,
    /// The port's decision for this slot.
    pub support: FeatureSupport,
}

/// Reason a [`FeatureProfile`] failed [`FeatureProfile::validate`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ProfileError {
    /// A non-supported decision carried an empty (or whitespace-only)
    /// justification; `field` names the offending slot.
    EmptyJustification {
        /// The [`FeatureEntry::name`] of the unjustified slot.
        field: &'static str,
    },
}

impl FeatureProfile {
    /// The two detection-source slots, in a stable order.
    #[must_use]
    pub const fn entries(&self) -> [FeatureEntry; 2] {
        [
            FeatureEntry {
                name: "isa_features",
                support: self.isa_features,
            },
            FeatureEntry {
                name: "core_identity",
                support: self.core_identity,
            },
        ]
    }

    /// Validate the honesty rule: every non-supported source carries a
    /// non-empty explanation.
    ///
    /// # Errors
    ///
    /// Returns [`ProfileError::EmptyJustification`] naming the first slot
    /// whose [`FeatureSupport::detail`] is present but empty.
    pub fn validate(&self) -> Result<(), ProfileError> {
        for entry in self.entries() {
            if let Some(reason) = entry.support.detail() {
                if reason.trim().is_empty() {
                    return Err(ProfileError::EmptyJustification { field: entry.name });
                }
            }
        }
        Ok(())
    }

    /// `true` if every source is release-ready — supported or a justified
    /// `Unsupported`, with no `Pending` gap remaining.
    #[must_use]
    pub fn is_release_ready(&self) -> bool {
        self.entries()
            .iter()
            .all(|entry| entry.support.is_release_ready())
    }
}

/// The CPU-feature-detection handle an architecture port exposes.
///
/// The kernel reads a per-CPU [`CpuFeatureSet`] and [`CoreType`] as each
/// CPU comes up and hands them to `lib/cpuops`, which selects the fastest
/// *correct* implementation of each accelerated operation for that core
/// type. Detection is keyed by [`CpuId`] because heterogeneous SMP means
/// per-CPU answers; a real port reads the register on the executing core,
/// so the `cpu` argument is the identity label the reads are attributed
/// to.
///
/// Implementations must be [`Send`] + [`Sync`]: the kernel reaches the
/// handle from every CPU.
pub trait CpuFeatures: Send + Sync {
    /// The set of ISA extensions `cpu` implements.
    ///
    /// Returns [`CpuFeatureSet::EMPTY`] when the port's
    /// [`FeatureProfile::isa_features`] is not
    /// [`FeatureSupport::Supported`] — an absent source fabricates no
    /// bit (fail closed).
    fn detect(&self, cpu: CpuId) -> CpuFeatureSet;

    /// The [`CoreType`] of `cpu`.
    ///
    /// Must be total and panic-free for every input, including an
    /// out-of-range [`CpuId`]; the safe answer is [`CoreType::UNKNOWN`].
    fn core_type(&self, cpu: CpuId) -> CoreType;

    /// The port's honest declaration of which detection sources it
    /// drives. Must satisfy [`FeatureProfile::validate`].
    fn profile(&self) -> FeatureProfile;
}

/// The CPU-feature-detection conformance vertical.
///
/// Every architecture port runs [`conformance::run_all`] against its
/// [`CpuFeatures`] handle. The suite is portable — it names only the
/// trait — and runs on the host, exactly like the [`super::memtag`]
/// vertical.
pub mod conformance {
    use super::{CpuFeatureSet, CpuFeatures, CpuId};

    /// Run the entire CPU-feature-detection conformance suite against
    /// `port`.
    ///
    /// # Panics
    ///
    /// Panics (failing the test) if any required property does not hold:
    /// the profile fails [`super::FeatureProfile::validate`], `detect` is
    /// unstable across back-to-back calls, a port declaring
    /// `isa_features` unsupported nonetheless reports a bit, or
    /// `core_type` is not total for an out-of-range [`CpuId`].
    pub fn run_all<C: CpuFeatures + ?Sized>(port: &C) {
        profile_is_honest(port);
        detect_is_stable(port);
        absent_source_reports_no_bit(port);
        core_type_is_total(port);
    }

    /// The profile validates and every non-supported source carries a
    /// non-empty justification.
    fn profile_is_honest<C: CpuFeatures + ?Sized>(port: &C) {
        let profile = port.profile();
        assert!(
            profile.validate().is_ok(),
            "feature profile must justify every non-supported source: {:?}",
            profile.validate()
        );
    }

    /// `detect` is stable: repeated reads for one [`CpuId`] agree (the
    /// capability gate must be a deterministic fact, never a race).
    fn detect_is_stable<C: CpuFeatures + ?Sized>(port: &C) {
        for cpu in [0 as CpuId, 1, CpuId::MAX] {
            let first = port.detect(cpu);
            assert_eq!(
                port.detect(cpu),
                first,
                "detect must be stable across back-to-back calls for cpu {cpu}"
            );
        }
    }

    /// A port whose `isa_features` source is not supported must report no
    /// extension at all (fail closed — never a fabricated bit).
    fn absent_source_reports_no_bit<C: CpuFeatures + ?Sized>(port: &C) {
        if !port.profile().isa_features.is_supported() {
            assert_eq!(
                port.detect(0),
                CpuFeatureSet::EMPTY,
                "a port without an ISA-feature source must fabricate no bit"
            );
        }
    }

    /// `core_type` is total: it returns a value for every input,
    /// including an out-of-range [`CpuId`], and never panics or disagrees
    /// with itself.
    fn core_type_is_total<C: CpuFeatures + ?Sized>(port: &C) {
        for cpu in [0 as CpuId, 1, CpuId::MAX] {
            let ct = port.core_type(cpu);
            assert_eq!(
                port.core_type(cpu),
                ct,
                "core_type must be a stable static identity for cpu {cpu}"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct StubPort {
        set: CpuFeatureSet,
        profile: FeatureProfile,
    }

    impl CpuFeatures for StubPort {
        fn detect(&self, _cpu: CpuId) -> CpuFeatureSet {
            if self.profile.isa_features.is_supported() {
                self.set
            } else {
                CpuFeatureSet::EMPTY
            }
        }
        fn core_type(&self, cpu: CpuId) -> CoreType {
            if cpu == CpuId::MAX {
                CoreType::UNKNOWN
            } else {
                CoreType {
                    model: Some("Stub Core"),
                    class: CoreClass::Performance,
                    raw_id: 0xABCD,
                }
            }
        }
        fn profile(&self) -> FeatureProfile {
            self.profile
        }
    }

    fn supported_profile() -> FeatureProfile {
        FeatureProfile {
            isa_features: FeatureSupport::Supported,
            core_identity: FeatureSupport::Supported,
        }
    }

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

    #[test]
    fn profile_validation_and_honesty() {
        assert_eq!(supported_profile().validate(), Ok(()));
        assert!(supported_profile().is_release_ready());

        let unsupported = FeatureProfile {
            isa_features: FeatureSupport::Unsupported("wasm host exposes no ISA extensions"),
            core_identity: FeatureSupport::Unsupported("wasm host exposes no CPU identity"),
        };
        assert_eq!(unsupported.validate(), Ok(()));
        assert!(unsupported.is_release_ready());

        let empty_reason = FeatureProfile {
            isa_features: FeatureSupport::Unsupported("  "),
            core_identity: FeatureSupport::Supported,
        };
        assert_eq!(
            empty_reason.validate(),
            Err(ProfileError::EmptyJustification {
                field: "isa_features"
            })
        );

        let pending = FeatureProfile {
            isa_features: FeatureSupport::Supported,
            core_identity: FeatureSupport::Pending("core-identity probe lands later"),
        };
        assert_eq!(pending.validate(), Ok(()));
        assert!(!pending.is_release_ready());
    }

    #[test]
    fn feature_support_helpers() {
        assert!(FeatureSupport::Supported.is_supported());
        assert!(FeatureSupport::Pending("x").is_pending());
        assert_eq!(FeatureSupport::Supported.detail(), None);
        assert_eq!(FeatureSupport::Unsupported("why").detail(), Some("why"));
        assert!(FeatureSupport::Unsupported("why").is_release_ready());
        assert!(!FeatureSupport::Pending("later").is_release_ready());
    }

    #[test]
    fn conformance_accepts_a_supported_port() {
        let port = StubPort {
            set: CpuFeatureSet::new().with(CpuFeature::Crc32),
            profile: supported_profile(),
        };
        conformance::run_all(&port);
        let dynamic: &dyn CpuFeatures = &port;
        conformance::run_all(dynamic);
    }

    #[test]
    fn conformance_accepts_an_unsupported_port() {
        let port = StubPort {
            set: CpuFeatureSet::EMPTY,
            profile: FeatureProfile {
                isa_features: FeatureSupport::Unsupported("no ISA source"),
                core_identity: FeatureSupport::Unsupported("no identity source"),
            },
        };
        conformance::run_all(&port);
    }

    #[test]
    #[should_panic(expected = "fabricate no bit")]
    fn conformance_rejects_a_fabricated_bit() {
        // Declares no ISA source yet returns a bit: the fail-closed
        // contract is violated and the suite must catch it.
        struct Liar;
        impl CpuFeatures for Liar {
            fn detect(&self, _cpu: CpuId) -> CpuFeatureSet {
                CpuFeatureSet::new().with(CpuFeature::Aes)
            }
            fn core_type(&self, _cpu: CpuId) -> CoreType {
                CoreType::UNKNOWN
            }
            fn profile(&self) -> FeatureProfile {
                FeatureProfile {
                    isa_features: FeatureSupport::Unsupported("no source"),
                    core_identity: FeatureSupport::Unsupported("no source"),
                }
            }
        }
        conformance::run_all(&Liar);
    }

    #[test]
    #[should_panic(expected = "must justify every non-supported source")]
    fn conformance_rejects_an_unjustified_claim() {
        let port = StubPort {
            set: CpuFeatureSet::EMPTY,
            profile: FeatureProfile {
                isa_features: FeatureSupport::Unsupported(""),
                core_identity: FeatureSupport::Supported,
            },
        };
        conformance::run_all(&port);
    }
}
