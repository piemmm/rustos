//! wasm32 CPU feature detection and cycle counter.
//!
//! Implements the Arch HAL
//! [`CpuFeatures`](tairix_arch_api::CpuFeatures) and
//! [`CpuCycles`](tairix_arch_api::CpuCycles) surfaces for the wasm32
//! host.
//!
//! A WebAssembly guest cannot see the host CPU's native instruction-set
//! extensions: there is no `CPUID`/`ID_AA64ISAR0_EL1`/`misa` to read, and
//! the WebAssembly SIMD proposal is a *module-level* capability the guest
//! is compiled with, not a runtime-detectable core feature. So the ISA
//! and core-identity detection sources are honestly
//! [`FeatureSupport::Unsupported`](tairix_arch_api::FeatureSupport::Unsupported),
//! detection reports the empty set, and `lib/cpuops` always selects the
//! portable baseline on this target (fail closed — never a fabricated
//! bit).
//!
//! The cycle counter is the host monotonic clock (`performance.now()`),
//! the same source the scheduler tick reads: a fixed-resolution,
//! monotonically-increasing time base sufficient for the bounded
//! `lib/cpuops` benchmark to rank two equally-correct portable routines.

use tairix_arch_api::{
    CoreType, CpuCycles, CpuFeatureSet, CpuFeatures, CpuId, FeatureProfile, FeatureSupport,
};

/// wasm32 implementation of the Arch HAL CPU-feature surface.
///
/// Zero-sized: there is no native ISA to read, so the handle carries no
/// state and detection always reports the empty set.
#[derive(Debug, Default, Clone, Copy)]
pub struct CpuFeatureDetect;

impl CpuFeatureDetect {
    /// Construct the wasm32 CPU-feature handle.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// The honest declaration for wasm32: neither detection source
    /// exists, each justified.
    #[must_use]
    pub const fn declared_profile() -> FeatureProfile {
        FeatureProfile {
            isa_features: FeatureSupport::Unsupported(
                "a WebAssembly guest cannot read the host CPU's native ISA extensions; SIMD is a \
                 module-level compile capability, not a runtime-detectable core feature",
            ),
            core_identity: FeatureSupport::Unsupported(
                "the wasm32 host exposes no per-core CPU identity register to the guest",
            ),
        }
    }
}

impl CpuFeatures for CpuFeatureDetect {
    fn detect(&self, _cpu: CpuId) -> CpuFeatureSet {
        // No native ISA visibility: fail closed to the empty set so the
        // dispatch framework always chooses the portable baseline.
        CpuFeatureSet::EMPTY
    }

    fn core_type(&self, _cpu: CpuId) -> CoreType {
        // No host CPU identity is exposed to the guest.
        CoreType::UNKNOWN
    }

    fn profile(&self) -> FeatureProfile {
        Self::declared_profile()
    }
}

/// wasm32 implementation of the Arch HAL cycle-counter surface (the host
/// `performance.now()` monotonic clock).
#[derive(Debug, Default, Clone, Copy)]
pub struct CpuCycleCounter;

impl CpuCycleCounter {
    /// Construct the wasm32 cycle-counter handle.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl CpuCycles for CpuCycleCounter {
    fn cpu_cycles(&self) -> u64 {
        // Reuse the one host-clock reader the scheduler tick uses, in
        // whole nanoseconds, so the benchmark and the clock can never
        // disagree on the time base (the host build substitutes a
        // strictly-increasing counter).
        crate::kernel_arch::ms_to_ns(crate::kernel_arch::read_now_ms())
    }

    fn cycles_monotonic_hint(&self) -> bool {
        // `performance.now()` is a monotonic, fixed-resolution wall clock:
        // a reliable (if coarse) constant-rate time base.
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tairix_arch_api::cpucycles;
    use tairix_arch_api::cpufeatures;

    #[test]
    fn detection_is_empty_and_profile_is_honest() {
        let handle = CpuFeatureDetect::new();
        assert_eq!(handle.detect(0), CpuFeatureSet::EMPTY);
        assert_eq!(handle.core_type(0), CoreType::UNKNOWN);
        let profile = handle.profile();
        assert_eq!(profile.validate(), Ok(()));
        // The sources are genuinely absent, so justified-Unsupported is
        // release-ready (no outstanding gap to wire up).
        assert!(profile.is_release_ready());
        assert!(!profile.isa_features.is_supported());
        assert!(!profile.core_identity.is_supported());
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
