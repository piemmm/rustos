//! riscv64 CPU feature detection and cycle counter.
//!
//! Implements the Arch HAL
//! [`CpuFeatures`](tairix_arch_api::CpuFeatures) and
//! [`CpuCycles`](tairix_arch_api::CpuCycles) surfaces for riscv64.
//!
//! RISC-V advertises its extensions to a kernel running in **S-mode**
//! through the device-tree `riscv,isa` string only: the base ISA and the
//! single-letter standard extensions (including the `V` vector extension)
//! and the multi-letter extensions (`Zbb`, `Zbc`, `Zbkc`, …) all appear
//! there. The `misa` CSR reports the same base/single-letter set, but it
//! is a **Machine-mode** CSR (address `0x301`): a `csrr misa` executed in
//! S-mode raises an illegal-instruction exception, so this port never
//! reads it (this is exactly how Linux discovers riscv extensions —
//! from the device tree, not `misa`). The handle carries the set parsed
//! from the ISA string the platform-discovery pass reads via
//! [`tairix_fdt`].
//!
//! The cycle counter is the architectural `time` CSR (`rdtime`) — a
//! fixed-rate, monotonically-increasing counter always available to
//! S-mode on the QEMU `virt` platform. Like the aarch64 `CNTVCT_EL0`
//! choice it is a wall-time proxy the `lib/cpuops` harness measures
//! deltas over, which is sufficient to rank two equally-correct
//! routines and needs no `rdcycle` counter-enable wiring.
//!
//! Only the architecture port parses the ISA string, so detection lives
//! here. The decoder (`features_from_isa_string`) is pure and host-tested.

use tairix_arch_api::{
    CoreType, CpuCycles, CpuFeature, CpuFeatureSet, CpuFeatures, CpuId, FeatureProfile,
    FeatureSupport,
};

/// Decode the standard extensions this port tracks from a
/// device-tree `riscv,isa` string (e.g. `"rv64imafdc_zba_zbb_zbc"`).
///
/// The RISC-V ISA-string grammar is lowercase, with the base and
/// single-letter extensions in the first `_`-separated token and each
/// multi-letter extension in its own subsequent token. This parser reads
/// the single-letter `V` bit from the first token and the
/// `Zbb`/`Zbc`/`Zbkc` tokens after it. An unrecognised token is ignored
/// (an honest "not one we gate on"), never guessed.
#[must_use]
pub fn features_from_isa_string(isa: &str) -> CpuFeatureSet {
    let mut set = CpuFeatureSet::EMPTY;
    let mut tokens = isa.split('_');
    if let Some(base) = tokens.next() {
        let single = base
            .strip_prefix("rv64")
            .or_else(|| base.strip_prefix("rv32"))
            .unwrap_or(base);
        if single.contains('v') {
            set = set.with(CpuFeature::VectorV);
        }
    }
    for token in tokens {
        match token {
            "zbb" => set = set.with(CpuFeature::Zbb),
            "zbc" => set = set.with(CpuFeature::Zbc),
            "zbkc" => set = set.with(CpuFeature::Zbkc),
            _ => {}
        }
    }
    set
}

/// riscv64 implementation of the Arch HAL CPU-feature surface.
///
/// Carries the extension set parsed from the device-tree `riscv,isa`
/// string — the only place a kernel running in S-mode can read them (the
/// `misa` CSR is Machine-mode-only, so `csrr misa` would raise an illegal
/// instruction at S-mode; this is exactly how Linux discovers riscv
/// extensions). A default-constructed handle carries no extensions — the
/// honest state before the device tree is read.
#[derive(Debug, Default, Clone, Copy)]
pub struct CpuFeatureDetect {
    from_isa_string: CpuFeatureSet,
}

impl CpuFeatureDetect {
    /// Construct a handle with no device-tree-derived extensions.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            from_isa_string: CpuFeatureSet::EMPTY,
        }
    }

    /// Construct a handle carrying the multi-letter extensions parsed
    /// from the device-tree `riscv,isa` string.
    #[must_use]
    pub fn from_isa_string(isa: &str) -> Self {
        Self {
            from_isa_string: features_from_isa_string(isa),
        }
    }

    /// The honest declaration for riscv64: the device-tree `riscv,isa`
    /// string covers the extensions this port gates on.
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
        // S-mode cannot read `misa` (an M-mode CSR), so every extension this
        // port gates on comes from the device-tree `riscv,isa` string the
        // handle was built from — never a faulting `csrr misa`.
        self.from_isa_string
    }

    fn core_type(&self, _cpu: CpuId) -> CoreType {
        // riscv64 has no compact single-register model identity like MIDR
        // or the CPUID signature; `mvendorid`/`marchid`/`mimpid` are
        // often zero on virtual platforms. Report the honest unknown —
        // the ops-table key falls back to the (homogeneous) default,
        // which is correct for the single-core-type targets today.
        CoreType::UNKNOWN
    }

    fn profile(&self) -> FeatureProfile {
        Self::declared_profile()
    }
}

/// riscv64 implementation of the Arch HAL cycle-counter surface (the
/// architectural `time` CSR read via `rdtime`).
#[derive(Debug, Default, Clone, Copy)]
pub struct CpuCycleCounter;

impl CpuCycleCounter {
    /// Construct the riscv64 cycle-counter handle.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl CpuCycles for CpuCycleCounter {
    fn cpu_cycles(&self) -> u64 {
        // Reuse the one `time`-CSR reader the monotonic clock uses, so the
        // benchmark and the clock can never disagree on the counter (the
        // host substitute is a strictly-increasing stub).
        crate::kernel_arch::read_time()
    }

    fn cycles_monotonic_hint(&self) -> bool {
        // The `time` CSR is the architectural fixed-frequency monotonic
        // counter: a reliable constant-rate time base by definition.
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tairix_arch_api::{cpucycles, cpufeatures};

    #[test]
    fn isa_string_multi_letter_extensions_decode() {
        let set = features_from_isa_string("rv64imafdc_zba_zbb_zbc_zbkc");
        assert!(set.contains(CpuFeature::Zbb));
        assert!(set.contains(CpuFeature::Zbc));
        assert!(set.contains(CpuFeature::Zbkc));
        // `zba` is not one we gate on, so it is silently ignored.
        assert!(!set.contains(CpuFeature::VectorV));
    }

    #[test]
    fn isa_string_single_letter_vector_decodes() {
        // The single-letter `v` in the base cluster is recognised.
        let set = features_from_isa_string("rv64imafdcv_zbb");
        assert!(set.contains(CpuFeature::VectorV));
        assert!(set.contains(CpuFeature::Zbb));
    }

    #[test]
    fn isa_string_without_gated_extensions_is_empty() {
        assert_eq!(features_from_isa_string("rv64imafdc"), CpuFeatureSet::EMPTY);
        assert_eq!(features_from_isa_string("rv64gc"), CpuFeatureSet::EMPTY);
    }

    #[test]
    fn handle_ors_string_derived_extensions() {
        // `detect` reflects exactly the string-derived set the handle was
        // built from (there is no `misa` read).
        let handle = CpuFeatureDetect::from_isa_string("rv64imafdc_zbb_zbc");
        let set = handle.detect(0);
        assert!(set.contains(CpuFeature::Zbb));
        assert!(set.contains(CpuFeature::Zbc));
        // A default handle carries nothing.
        assert_eq!(CpuFeatureDetect::new().detect(0), CpuFeatureSet::EMPTY);
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
        // A handle with string-derived extensions is also conformant.
        cpufeatures::conformance::run_all(&CpuFeatureDetect::from_isa_string("rv64gc_zbb"));
        let dynamic: &dyn CpuFeatures = &CpuFeatureDetect::new();
        cpufeatures::conformance::run_all(dynamic);
    }

    #[test]
    fn passes_cpucycles_conformance() {
        cpucycles::conformance::run_all(&CpuCycleCounter::new());
        assert!(CpuCycleCounter::new().cycles_monotonic_hint());
    }
}
