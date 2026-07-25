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

// The arch-neutral capability vocabulary — [`CpuFeature`] and
// [`CpuFeatureSet`] — is defined once in the dependency-free ABI crate
// (`tairix_abi::cpufeatures`) so that both this HAL (which *produces* the
// set from ID registers) and the generic `lib/cpuops` dispatch framework
// (which *consumes* it) share one definition without `lib/cpuops` acquiring
// a forbidden edge to `kernel/*`. It is re-exported here so ports and
// kernel consumers keep naming it through the HAL, exactly as the hardware
// tree (`tairix_abi::hwtree`) is reached through the platform slice.
pub use tairix_abi::cpufeatures::{CpuFeature, CpuFeatureSet};

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

    // The `CpuFeature`/`CpuFeatureSet` bit-layout and membership tests live
    // beside their definition in `tairix_abi::cpufeatures`; only the
    // HAL-specific surface (profile honesty, the `CpuFeatures` trait, and the
    // conformance vertical) is tested here.

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
